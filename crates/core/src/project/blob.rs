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

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::UNIX_EPOCH;

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

/// The zstd level every blob is written at. The Z-Map is piecewise-constant
/// and compresses by orders of magnitude at any level; the composite is
/// photographic and barely compresses. On a 61-frame 7795x5208 composite
/// (243.6 MB) multithreaded level 1 took 0.218 s for ratio 1.150, level 2
/// 0.242 s for 1.150 and level 3 0.255 s for 1.171: level 1 is the fastest
/// and within 2 % of level 3's size.
pub const ZSTD_LEVEL: i32 = 1;

/// A zstd encoder writing to a new file at `path`, one worker per available
/// core. The worker count never changes the entry name: the hash is over the
/// uncompressed bytes.
fn encoder(
    path: &Path,
    level: i32,
) -> Result<zstd::stream::write::Encoder<'static, File>, ProjectError> {
    let file = File::create(path).map_err(|source| ProjectError::io(path, source))?;
    let mut encoder = zstd::stream::write::Encoder::new(file, level)
        .map_err(|source| ProjectError::io(path, source))?;
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    encoder
        .multithread(workers)
        .map_err(|source| ProjectError::io(path, source))?;
    Ok(encoder)
}

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

/// What identifies one version of a file without reading it: size,
/// modification time and, where the platform has one, the device and inode.
///
/// An in-place edit that keeps all three goes unnoticed; that is the price
/// of not reading every source on every open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub bytes: u64,
    /// Nanoseconds since the Unix epoch; `0` for a time before it.
    pub modified_ns: u64,
    /// `[device, inode]` on Unix, `None` elsewhere.
    pub file_id: Option<[u64; 2]>,
}

impl FileStamp {
    pub fn of(path: &Path) -> Result<Self, ProjectError> {
        let io = |source| ProjectError::io(path, source);
        let metadata = std::fs::metadata(path).map_err(io)?;
        let modified = metadata.modified().map_err(io)?;
        #[cfg(unix)]
        let file_id = {
            use std::os::unix::fs::MetadataExt;
            Some([metadata.dev(), metadata.ino()])
        };
        #[cfg(not(unix))]
        let file_id = None;
        Ok(FileStamp {
            bytes: metadata.len(),
            modified_ns: modified
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64),
            file_id,
        })
    }
}

/// The change time (`ctime`, nanoseconds) a memoised hash was taken at;
/// `None` where the platform has no such time.
type ChangeTime = Option<i128>;

fn change_time(path: &Path) -> Result<ChangeTime, ProjectError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).map_err(|source| ProjectError::io(path, source))?;
        Ok(Some(
            i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec()),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

/// Every source hashed in this process, by the path it was hashed at.
static SOURCE_HASHES: LazyLock<Mutex<HashMap<PathBuf, MemoEntry>>> = LazyLock::new(Mutex::default);

/// A memoised hash with the stamp and change time it was taken at.
type MemoEntry = (FileStamp, ChangeTime, String);

/// The current stamp of the file at `path` and its blake3 content hash, hex.
///
/// The hash comes from `recorded` when its stamp matches, else from this
/// process's memo at the same stamp, else from reading the file. The memo
/// lock is held through that read: sources are hashed one at a time
/// process-wide, and never twice by two racing callers.
///
/// Like a project's own checks, this trusts the stamp: an in-place edit that
/// keeps size, modification time and inode keeps the remembered hash. Use
/// [`source_hash_change_checked`] where that must not hold.
pub fn source_hash(
    path: &Path,
    recorded: Option<&SourceRef>,
) -> Result<(FileStamp, String), ProjectError> {
    let stamp = FileStamp::of(path)?;
    if let Some(recorded) = recorded.filter(|r| r.stamp() == stamp) {
        return Ok((stamp, recorded.hash.clone()));
    }
    memoised_hash(path, stamp, false)
}

/// [`source_hash`] without a recorded hash, and with the memo's hash reused
/// only if the file's change time (`ctime`) is also unchanged, so an in-place
/// rewrite that restores size and modification time is hashed again.
///
/// Without a change time (non-Unix), such a rewrite within one process still
/// reuses the memoised hash.
pub fn source_hash_change_checked(path: &Path) -> Result<(FileStamp, String), ProjectError> {
    memoised_hash(path, FileStamp::of(path)?, true)
}

/// The memoised hash at `stamp` (and, when `by_change_time`, the file's
/// current change time), else a fresh read stored in the memo. A reused hash
/// leaves the entry as it is, so its change time stays the one the hash was
/// taken at.
fn memoised_hash(
    path: &Path,
    stamp: FileStamp,
    by_change_time: bool,
) -> Result<(FileStamp, String), ProjectError> {
    let changed = change_time(path)?;
    let mut memo = SOURCE_HASHES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((memoised, at, hash)) = memo.get(path)
        && *memoised == stamp
        && (!by_change_time || *at == changed)
    {
        return Ok((stamp, hash.clone()));
    }
    let hash = hash_file(path)?;
    memo.insert(path.to_path_buf(), (stamp, changed, hash.clone()));
    Ok((stamp, hash))
}

/// blake3 of a whole file, hex.
///
/// `update_mmap_rayon` maps the **whole** file, so this must never run
/// inside a `par_iter`: 61 reference frames hashed concurrently would map up
/// to 16 GB at once, well past an 8 GB memory ceiling. One file at a time is
/// one 268 MB mapping, and the hashing of that one file is already
/// rayon-parallel internally.
fn hash_file(path: &Path) -> Result<String, ProjectError> {
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
    let mut encoder = encoder(&temp, level)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn source(hash: &str) -> SourceRef {
        SourceRef {
            path: "x.tif".to_string(),
            resolution: super::super::manifest::SourceResolution::Relative,
            hash: hash.to_string(),
            bytes: 1,
            modified_ns: 0,
            file_id: None,
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

    #[cfg(unix)]
    #[test]
    fn an_in_place_rewrite_with_a_restored_mtime_is_seen_only_by_the_change_checked_hash() {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.bin");
        std::fs::write(&path, [7u8; 4096]).unwrap();
        let (_, before) = source_hash(&path, None).unwrap();

        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[9u8], 100).unwrap();
        file.set_modified(modified).unwrap();
        drop(file);

        // The stamp-only call reuses the old hash and leaves the memo's
        // change time alone, or the checked call would trust it afterwards.
        assert_eq!(source_hash(&path, None).unwrap().1, before, "stamp only");
        let (_, after) = source_hash_change_checked(&path).unwrap();
        assert_ne!(after, before, "the change time moved");
        assert_eq!(source_hash_change_checked(&path).unwrap().1, after);
        assert_eq!(source_hash(&path, None).unwrap().1, after);
    }
}
