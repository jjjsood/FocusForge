// SPDX-License-Identifier: GPL-3.0-or-later

//! Hashing and the content-addressed blob store.
//!
//! **The hash is over the uncompressed bytes.** That is what makes the zstd
//! level a free parameter: recompressing the
//! same plane at a different level produces the same entry name, so a later
//! build may tune compression without orphaning every blob ever written.
//!
//! **One decode pass, two files.** [`stage_plane`] reads the staged plane
//! back in horizontal bands -- never a whole-frame read, `crate::io`'s
//! standing rule -- feeding each band to a `blake3::Hasher` *and* to a zstd
//! encoder at the same time. The hash is known when the band loop ends,
//! which is precisely what makes the zip entry name computable; the
//! compressed temp file is then copied into the container under
//! `blobs/<hash>.zst`.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::error::ProjectError;
use super::manifest::SourceRef;
use crate::io::TiffStack;
use crate::tile::Rect;
use crate::zmap::ZMapReader;

/// Which of a project's two payload planes is meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plane {
    Composite,
    ZMap,
}

impl Plane {
    /// The stem used for this plane's compressed temp file. Never the
    /// entry name: that is the content hash, unknown until the last band.
    fn stem(self) -> &'static str {
        match self {
            Plane::Composite => "composite",
            Plane::ZMap => "zmap",
        }
    }
}

/// Roughly how many bytes one band holds. Bounded work, bounded memory --
/// the same discipline the read path uses.
const BAND_BYTES: usize = 4 << 20;

/// The default zstd level. Level 3 is zstd's own default: the Z-Map is
/// piecewise-constant and compresses by orders of magnitude at any level,
/// and the composite is photographic and compresses by very little at any
/// level, so buying seconds of CPU for a higher level buys nothing here.
pub const ZSTD_LEVEL: i32 = 3;

/// One compressed plane sitting in a temp file, ready to be copied into a
/// container under `blobs/<hash>.zst`.
#[derive(Debug, Clone)]
pub struct StagedBlob {
    /// blake3 of the **uncompressed** bytes, hex.
    pub hash: String,
    pub uncompressed_len: u64,
    pub compressed_len: u64,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    /// The compressed temp file. Removed by [`StagedBlob::remove`].
    pub path: PathBuf,
}

impl StagedBlob {
    /// Best-effort removal of the compressed temp file.
    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// blake3 of a whole file, hex.
///
/// `update_mmap_rayon` maps the **whole** file, so this must be called
/// sequentially over a source list and never inside a `par_iter`: 61
/// reference frames hashed concurrently would map up to 16 GB at once,
/// well past an 8 GB memory ceiling. One file at a time is one 268 MB
/// mapping, and the hashing of that one file is already rayon-parallel
/// internally.
pub fn hash_file(path: &Path) -> Result<String, ProjectError> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap_rayon(path)
        .map_err(|source| ProjectError::io(path, source))?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// The project's stack fingerprint: blake3 over the frame count followed by
/// each source's own content hash, in frame order.
///
/// A **real content hash**, deliberately different from the cheap
/// header-only [`crate::job::StackFingerprint`]. That one exists to find a
/// resumable ledger in milliseconds without reading a pixel; this one
/// exists so a project can say, years later, exactly which bytes it was
/// computed from. The two are different values by design and neither
/// substitutes for the other.
pub fn stack_fingerprint(sources: &[SourceRef]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(sources.len() as u64).to_le_bytes());
    for source in sources {
        hasher.update(source.hash.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// A short, stable, filesystem-safe digest of `input` -- 16 hex characters
/// of blake3.
///
/// Used by `crates/cli` to key one run's staging directory by the project
/// path it will seal into. Stable across runs (the same invocation produces
/// the same key, which is what `--resume` needs to find its ledger) and
/// distinct per output (two runs collide exactly when they already share
/// an output path).
pub fn short_hash(input: &str) -> String {
    blake3::hash(input.as_bytes()).to_hex()[..16].to_string()
}

/// Reads a staged plane back band by band, hashing the uncompressed bytes
/// and zstd-compressing them into `<out_dir>/<plane>.zst` in the same pass.
///
/// `level` is a free parameter precisely because the hash is taken over the
/// uncompressed bytes: two levels produce two differently-sized files with
/// one entry name.
pub fn stage_plane(
    source: &Path,
    plane: Plane,
    level: i32,
    out_dir: &Path,
) -> Result<StagedBlob, ProjectError> {
    let reader = PlaneReader::open(source, plane)?;
    let (width, height, spp) = (reader.width, reader.height, reader.samples_per_pixel);

    let row_samples = width as usize * spp as usize;
    let rows = (BAND_BYTES / (row_samples * 2)).clamp(1, height as usize) as u32;

    let temp = out_dir.join(format!("{}.zst", plane.stem()));
    let file = File::create(&temp).map_err(|source| ProjectError::io(&temp, source))?;
    let mut encoder = zstd::stream::write::Encoder::new(file, level)
        .map_err(|source| ProjectError::io(&temp, source))?;

    let mut hasher = blake3::Hasher::new();
    let mut samples = vec![0u16; row_samples * rows as usize];
    let mut bytes: Vec<u8> = Vec::with_capacity(row_samples * rows as usize * 2);
    let mut uncompressed_len = 0u64;

    let mut y = 0;
    while y < height {
        let band_rows = rows.min(height - y);
        let len = row_samples * band_rows as usize;
        let rect = Rect {
            x: 0,
            y,
            width,
            height: band_rows,
        };
        reader.read_band(rect, &mut samples[..len])?;
        bytes.clear();
        for sample in &samples[..len] {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        hasher.update(&bytes);
        encoder
            .write_all(&bytes)
            .map_err(|source| ProjectError::io(&temp, source))?;
        uncompressed_len += bytes.len() as u64;
        y += band_rows;
    }

    let file = encoder
        .finish()
        .map_err(|source| ProjectError::io(&temp, source))?;
    let compressed_len = file
        .metadata()
        .map_err(|source| ProjectError::io(&temp, source))?
        .len();
    drop(file);

    Ok(StagedBlob {
        hash: hasher.finalize().to_hex().to_string(),
        uncompressed_len,
        compressed_len,
        width,
        height,
        samples_per_pixel: spp,
        path: temp,
    })
}

/// A staged plane, whichever of the two it is: the one place the composite's
/// `TiffStack` and the Z-Map's `ZMapReader` are unified, so [`stage_plane`]
/// has a single band loop rather than two near-copies.
enum PlaneSource {
    Composite(Box<TiffStack>),
    ZMap(Box<ZMapReader>),
}

struct PlaneReader {
    source: PlaneSource,
    path: PathBuf,
    width: u32,
    height: u32,
    samples_per_pixel: u32,
}

impl PlaneReader {
    fn open(path: &Path, plane: Plane) -> Result<Self, ProjectError> {
        match plane {
            Plane::Composite => {
                let stack = TiffStack::open_files(std::slice::from_ref(&path.to_path_buf()))
                    .map_err(|err| ProjectError::container(path, err))?;
                let (width, height) = stack.dimensions();
                let samples_per_pixel = stack.header(0).samples_per_pixel;
                Ok(PlaneReader {
                    source: PlaneSource::Composite(Box::new(stack)),
                    path: path.to_path_buf(),
                    width,
                    height,
                    samples_per_pixel,
                })
            }
            Plane::ZMap => {
                let reader =
                    ZMapReader::open(path).map_err(|err| ProjectError::container(path, err))?;
                let (width, height) = reader.dimensions();
                Ok(PlaneReader {
                    source: PlaneSource::ZMap(Box::new(reader)),
                    path: path.to_path_buf(),
                    width,
                    height,
                    samples_per_pixel: 1,
                })
            }
        }
    }

    fn read_band(&self, rect: Rect, out: &mut [u16]) -> Result<(), ProjectError> {
        match &self.source {
            PlaneSource::Composite(stack) => stack
                .read_tile_into(0, rect, out)
                .map_err(|err| ProjectError::container(&self.path, err)),
            PlaneSource::ZMap(reader) => reader
                .read_tile_into(rect, out)
                .map_err(|err| ProjectError::container(&self.path, err)),
        }
    }

    /// The source-frame count a Z-Map declares; `None` for a composite,
    /// which carries no such fact of its own.
    fn frame_count(&self) -> Option<u32> {
        match &self.source {
            PlaneSource::Composite(_) => None,
            PlaneSource::ZMap(reader) => Some(reader.frame_count()),
        }
    }
}

/// The source-frame count a staged Z-Map declares, read from its header
/// alone.
pub fn zmap_frame_count(path: &Path) -> Result<u32, ProjectError> {
    PlaneReader::open(path, Plane::ZMap)?
        .frame_count()
        .ok_or_else(|| ProjectError::container(path, "not a Z-Map"))
}

/// The uncompressed byte length of a patch's own header: the declared map's
/// width and height, then the patched tile's `x`, `y`, `width` and `height`
/// -- six little-endian `u32`s, before the tile's own `u16` indices.
const DELTA_HEADER_LEN: usize = 6 * 4;

/// One [`stage_delta`]/[`decode_delta`] payload: a rectangular tile of new
/// indices plus the full map's own declared extent, so a decode can be
/// checked against a project's actual Z-Map shape before it is applied.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedDelta {
    pub map_width: u32,
    pub map_height: u32,
    pub tile: Rect,
    /// Row-major over `tile`, `tile.width * tile.height` long.
    pub indices: Vec<u16>,
}

/// Encodes `indices` (`tile`'s own indices, row-major, `tile.width *
/// tile.height` long) as a patch against a `map_width x map_height` map,
/// hashing and zstd-compressing it in one banded pass exactly as
/// [`stage_plane`] does. The hash covers the header too, so two patches
/// naming the same tile bytes at different positions never collide.
pub fn stage_delta(
    map_width: u32,
    map_height: u32,
    tile: Rect,
    indices: &[u16],
    level: i32,
    out_dir: &Path,
) -> Result<StagedBlob, ProjectError> {
    let temp = out_dir.join("zmap-delta.zst");
    let file = File::create(&temp).map_err(|source| ProjectError::io(&temp, source))?;
    let mut encoder = zstd::stream::write::Encoder::new(file, level)
        .map_err(|source| ProjectError::io(&temp, source))?;
    let mut hasher = blake3::Hasher::new();

    let mut header = [0u8; DELTA_HEADER_LEN];
    for (i, v) in [
        map_width,
        map_height,
        tile.x,
        tile.y,
        tile.width,
        tile.height,
    ]
    .iter()
    .enumerate()
    {
        header[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    hasher.update(&header);
    encoder
        .write_all(&header)
        .map_err(|source| ProjectError::io(&temp, source))?;
    let mut uncompressed_len = header.len() as u64;

    let row_samples = tile.width as usize;
    let rows = (BAND_BYTES / (row_samples.max(1) * 2)).clamp(1, tile.height.max(1) as usize);
    let mut bytes: Vec<u8> = Vec::with_capacity(row_samples * rows * 2);
    let mut y = 0usize;
    let total_rows = tile.height as usize;
    while y < total_rows {
        let band_rows = rows.min(total_rows - y);
        let start = y * row_samples;
        let len = row_samples * band_rows;
        bytes.clear();
        for sample in &indices[start..start + len] {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        hasher.update(&bytes);
        encoder
            .write_all(&bytes)
            .map_err(|source| ProjectError::io(&temp, source))?;
        uncompressed_len += bytes.len() as u64;
        y += band_rows;
    }

    let file = encoder
        .finish()
        .map_err(|source| ProjectError::io(&temp, source))?;
    let compressed_len = file
        .metadata()
        .map_err(|source| ProjectError::io(&temp, source))?
        .len();
    drop(file);

    Ok(StagedBlob {
        hash: hasher.finalize().to_hex().to_string(),
        uncompressed_len,
        compressed_len,
        width: map_width,
        height: map_height,
        samples_per_pixel: 1,
        path: temp,
    })
}

/// Decodes a [`stage_delta`] payload's **uncompressed** bytes, refusing a
/// length that does not match its own header or a tile that does not lie
/// inside its own declared map. `path` names the failure only -- callers
/// pass the project file, not this blob's temp path.
pub fn decode_delta(bytes: &[u8], path: &Path) -> Result<DecodedDelta, ProjectError> {
    if bytes.len() < DELTA_HEADER_LEN {
        return Err(ProjectError::container(
            path,
            format!(
                "a stored patch is {} bytes, shorter than its own {DELTA_HEADER_LEN}-byte header",
                bytes.len()
            ),
        ));
    }
    let read_u32 = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    let map_width = read_u32(0);
    let map_height = read_u32(1);
    let tile = Rect {
        x: read_u32(2),
        y: read_u32(3),
        width: read_u32(4),
        height: read_u32(5),
    };

    let in_bounds = tile.width > 0
        && tile.height > 0
        && tile
            .x
            .checked_add(tile.width)
            .is_some_and(|xw| xw <= map_width)
        && tile
            .y
            .checked_add(tile.height)
            .is_some_and(|yh| yh <= map_height);
    if !in_bounds {
        return Err(ProjectError::container(
            path,
            format!(
                "a stored patch's tile {tile:?} lies outside its own {map_width}x{map_height} map"
            ),
        ));
    }

    let expected_len = DELTA_HEADER_LEN + tile.width as usize * tile.height as usize * 2;
    if bytes.len() != expected_len {
        return Err(ProjectError::container(
            path,
            format!(
                "a stored patch holds {} bytes, not the {expected_len} its own header declares",
                bytes.len()
            ),
        ));
    }

    let mut indices = vec![0u16; tile.width as usize * tile.height as usize];
    for (out, chunk) in indices
        .iter_mut()
        .zip(bytes[DELTA_HEADER_LEN..].chunks_exact(2))
    {
        *out = u16::from_le_bytes([chunk[0], chunk[1]]);
    }

    Ok(DecodedDelta {
        map_width,
        map_height,
        tile,
        indices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(hash: &str) -> SourceRef {
        SourceRef {
            path: "x.tif".to_string(),
            resolution: super::super::manifest::SourceResolution::Relative,
            hash: hash.to_string(),
            bytes: 1,
        }
    }

    #[test]
    fn the_fingerprint_follows_the_frame_order_and_the_count() {
        let a = stack_fingerprint(&[source("aa"), source("bb")]);
        assert_eq!(a, stack_fingerprint(&[source("aa"), source("bb")]));
        assert_ne!(a, stack_fingerprint(&[source("bb"), source("aa")]));
        assert_ne!(a, stack_fingerprint(&[source("aa")]));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn empty_and_one_frame_stacks_have_different_fingerprints() {
        // Stated as an assertion rather than only in prose: the project's
        // fingerprint is derived from content hashes, so an empty stack and
        // a one-frame stack cannot collide the way a header digest might.
        assert_ne!(stack_fingerprint(&[]), stack_fingerprint(&[source("")]));
    }

    fn roundtrip(map_width: u32, map_height: u32, tile: Rect) {
        let dir = tempfile::tempdir().unwrap();
        let indices: Vec<u16> = (0..tile.width * tile.height)
            .map(|i| (i % 40) as u16)
            .collect();
        let staged = stage_delta(map_width, map_height, tile, &indices, 3, dir.path()).unwrap();
        let compressed = std::fs::read(&staged.path).unwrap();
        let bytes = zstd::stream::decode_all(&compressed[..]).unwrap();
        assert_eq!(bytes.len(), staged.uncompressed_len as usize);
        let decoded = decode_delta(&bytes, Path::new("test.ffp")).unwrap();
        assert_eq!(decoded.map_width, map_width);
        assert_eq!(decoded.map_height, map_height);
        assert_eq!(decoded.tile, tile);
        assert_eq!(decoded.indices, indices);
    }

    #[test]
    fn a_tile_at_the_origin_round_trips() {
        roundtrip(
            37,
            23,
            Rect {
                x: 0,
                y: 0,
                width: 5,
                height: 4,
            },
        );
    }

    #[test]
    fn a_tile_on_the_far_edges_of_an_odd_sized_map_round_trips() {
        roundtrip(
            37,
            23,
            Rect {
                x: 30,
                y: 18,
                width: 7,
                height: 5,
            },
        );
    }

    #[test]
    fn a_whole_map_tile_round_trips() {
        roundtrip(
            37,
            23,
            Rect {
                x: 0,
                y: 0,
                width: 37,
                height: 23,
            },
        );
    }

    #[test]
    fn decode_delta_refuses_bytes_truncated_before_the_header() {
        match decode_delta(&[0u8; 4], Path::new("test.ffp")) {
            Err(ProjectError::Container { message, .. }) => {
                assert!(message.contains("header"), "{message}");
            }
            other => panic!("expected a Container refusal, got {other:?}"),
        }
    }

    #[test]
    fn decode_delta_refuses_a_tile_outside_its_own_map() {
        let mut header = [0u8; DELTA_HEADER_LEN];
        for (i, v) in [10u32, 10, 8, 8, 5, 5].iter().enumerate() {
            header[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let bytes = [header.to_vec(), vec![0u8; 5 * 5 * 2]].concat();
        match decode_delta(&bytes, Path::new("test.ffp")) {
            Err(ProjectError::Container { message, .. }) => {
                assert!(message.contains("outside"), "{message}");
            }
            other => panic!("expected a Container refusal, got {other:?}"),
        }
    }

    #[test]
    fn decode_delta_refuses_a_length_that_does_not_match_its_own_header() {
        let mut header = [0u8; DELTA_HEADER_LEN];
        for (i, v) in [10u32, 10, 0, 0, 5, 5].iter().enumerate() {
            header[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let bytes = [header.to_vec(), vec![0u8; 5 * 5 * 2 - 2]].concat();
        match decode_delta(&bytes, Path::new("test.ffp")) {
            Err(ProjectError::Container { message, .. }) => {
                assert!(message.contains("bytes"), "{message}");
            }
            other => panic!("expected a Container refusal, got {other:?}"),
        }
    }
}
