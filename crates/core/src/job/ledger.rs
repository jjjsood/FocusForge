// SPDX-License-Identifier: GPL-3.0-or-later

//! [`ResumeStore`] and [`Ledger`]: the durable completion ledger under
//! `<scratch_base>/resume/`, and the ordering rule that makes it crash-safe
//! by construction with no per-unit `fsync`.
//!
//! # The resume path, end to end
//!
//! This is the composed sequence a `--resume` run calls, and the
//! sequence `crates/core/tests/job_resume.rs` exercises: compute a
//! [`JobRecord`] for the stack and parameters about to run, call
//! [`ResumeStore::find`]. On [`Resumable::Mismatch`], stop and surface the
//! typed error -- never mix old intermediates with new frames. On
//! [`Resumable::None`], call [`ResumeStore::begin`] and run normally, every
//! unit recorded as it completes through a [`RecordingConsumer`]. On
//! [`Resumable::Ledger`], filter [`Ledger::completed`] through
//! `TiffWriter::holds_rows` (a recorded unit the output file
//! cannot hold is dropped back into the recomputed set), reopen both output
//! writers with [`crate::fuse::FusionConsumer::reopen`] seeded with the
//! surviving units' rects, build the job with [`super::Job::resuming`] over
//! the surviving indices, and -- when [`Ledger::alignment`] already holds a
//! recorded [`crate::align::StackAlignment`] -- skip `align_stack` entirely
//! and reuse it.
//!
//! # The ordering rule
//!
//! [`Ledger::record_unit`] is called only after a tile's rows have already
//! reached the OS through [`crate::io::TiffWriter::write_tile`] --
//! [`RecordingConsumer::consume`] enforces this by calling the inner
//! consumer (which writes the tile) **before** recording the unit, never the
//! reverse. The reverse order would let a crash between the two writes
//! produce a resumed run that skips a tile it never actually wrote: the
//! ledger would claim the unit complete while the output file still held a
//! hole of zeros there. With no per-unit `fsync`, this ordering buys full
//! safety against a **process** crash (a process crash cannot reorder two
//! writes that already both happened, in program order, against the same
//! page cache) and bounded damage against a **power cut** (which can lose
//! either write's durability independently of program order) --
//! `TiffWriter::holds_rows` is what catches that remaining
//! case on the way back in, by treating a ledger entry the output file
//! cannot hold as incomplete.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::algorithm::AlgorithmError;
use crate::align::{ReferencePolicy, StackAlignment};
use crate::tile::scratch::unique_child_dir;
use crate::tile::{TileConsumer, TileJob};

use super::fingerprint::{FingerprintMismatch, FrameCount};
use super::record::JobRecord;
use super::stage_cache::CachedFrameAlignment;

/// Everything that can go wrong opening a [`ResumeStore`], beginning or
/// finding a [`Ledger`], or recording into one.
#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{}: {source}", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// The candidate ledger [`ResumeStore::find`] chose disagrees with the
    /// freshly computed identity -- either a per-frame fingerprint
    /// disagreement or a changed algorithm/parameter/geometry field.
    #[error("{0}")]
    Mismatch(FingerprintMismatch),
}

/// What [`ResumeStore::find`] returns for one freshly computed [`JobRecord`].
#[derive(Debug)]
pub enum Resumable {
    /// Nothing under the store matches -- run normally through
    /// [`ResumeStore::begin`].
    None,
    /// A candidate matched enough to be *the* intended resume target but
    /// disagrees on some identity field -- refuse loudly, never mix old
    /// intermediates with new frames.
    Mismatch(FingerprintMismatch),
    /// A candidate matches in full: its ledger is ready to drive a resume.
    Ledger(Ledger),
}

/// One line of `units.log`: `{"unit":<index>}\n`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct UnitRecord {
    unit: usize,
}

/// A run-scoped resume directory under `<scratch_base>/resume/`: the record
/// it was begun with (implicitly, via `job.json` on disk), the append-only
/// completion log, and -- once recorded -- the finished [`StackAlignment`].
#[derive(Debug)]
pub struct Ledger {
    dir: PathBuf,
    units_log: Mutex<File>,
    completed: Vec<usize>,
    alignment: Option<StackAlignment>,
}

impl Ledger {
    /// Opens (creating if absent) `dir/units.log` for append and reads back
    /// whatever it already holds -- the same code path serves a freshly
    /// [`ResumeStore::begin`]'d directory (nothing to read yet) and a
    /// [`ResumeStore::find`]'d one (a resume's recorded units).
    fn open(dir: PathBuf) -> Result<Self, ResumeError> {
        let units_log_path = dir.join("units.log");
        let completed = read_completed_units(&units_log_path)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&units_log_path)
            .map_err(|source| ResumeError::Io {
                path: units_log_path.clone(),
                source,
            })?;
        let alignment = read_alignment(&dir.join("alignment.json"))?;
        Ok(Ledger {
            dir,
            units_log: Mutex::new(file),
            completed,
            alignment,
        })
    }

    /// This directory's path under `<scratch_base>/resume/`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Call only after the unit's own rows have already reached
    /// [`crate::io::TiffWriter::write_tile`] -- see the module docs' "The
    /// ordering rule" for why.
    pub fn record_unit(&self, index: usize) -> Result<(), ResumeError> {
        let mut line = serde_json::to_string(&UnitRecord { unit: index }).map_err(|source| {
            ResumeError::Json {
                path: self.dir.join("units.log"),
                source,
            }
        })?;
        line.push('\n');
        let mut file = self.units_log.lock().unwrap();
        file.write_all(line.as_bytes())
            .map_err(|source| ResumeError::Io {
                path: self.dir.join("units.log"),
                source,
            })
    }

    /// Writes `alignment.json` atomically, once, at the align/fuse
    /// boundary, through a bit-exact `f64::to_bits()` encoding rather than
    /// plain JSON floats: without it, `serde_json`'s float formatter is not
    /// guaranteed to round-trip a 17-significant-digit `f64` to the same
    /// bits (<https://github.com/serde-rs/json/issues/707>), so a resumed
    /// run's transforms could in principle drift by 1 ULP from the
    /// interrupted run's.
    pub fn record_alignment(&self, alignment: &StackAlignment) -> Result<(), ResumeError> {
        let encoded = LedgerAlignment {
            reference: alignment.reference,
            policy: alignment.policy,
            frames: alignment
                .frames
                .iter()
                .map(CachedFrameAlignment::from)
                .collect(),
        };
        write_atomic(&self.dir.join("alignment.json"), &encoded)
    }

    /// The recorded [`StackAlignment`], if this ledger's run had already
    /// finished aligning -- `None` on a fresh (just-begun) ledger, or a
    /// ledger whose resumed run had not yet reached the fuse stage.
    pub fn alignment(&self) -> Option<&StackAlignment> {
        self.alignment.as_ref()
    }

    /// The [`crate::tile::plan::PlannedTile::index`] values this ledger
    /// recorded as complete, as read back from `units.log` when this
    /// `Ledger` was opened. Empty on a freshly begun ledger.
    pub fn completed(&self) -> &[usize] {
        &self.completed
    }

    /// Drops the open `units.log` handle and returns this ledger's
    /// directory -- the handle must close before anything removes the
    /// directory it lives in, the same ordering `write_atomic` uses between
    /// its temp file and its rename.
    fn close(self) -> PathBuf {
        let Ledger { dir, units_log, .. } = self;
        drop(units_log);
        dir
    }

    /// Removes this ledger's whole directory -- called by a caller (the
    /// CLI) once its run's `finish()` has succeeded. A cancelled or crashed
    /// run's ledger is never discarded here: only a caller that knows its
    /// own run is genuinely done calls this. Closes the `units.log` handle
    /// before removing the directory, so a filesystem that keeps an
    /// unlinked-but-open file visible cannot see the directory as non-empty.
    pub fn discard(self) -> Result<(), ResumeError> {
        let dir = self.close();
        fs::remove_dir_all(&dir).map_err(|source| ResumeError::Io { path: dir, source })
    }
}

/// The text of `path`, or `None` when the file does not exist.
fn read_opt(path: &Path) -> Result<Option<String>, ResumeError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ResumeError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Read `path` back as the sorted set of completed unit indices, tolerating
/// a torn trailing record.
///
/// **Newline-framed, deliberately, not `StreamDeserializer`**
/// ([serde-rs/json#524](https://github.com/serde-rs/json/issues/524),
/// closed as working-as-intended: `StreamDeserializer` reports a cut-off
/// numeric literal as a hard parse error rather than EOF, and a ledger of
/// bare integers is exactly that hazard). Only bytes up to and including
/// the **last** `\n` are ever considered; anything after it is an
/// in-progress write with no durability guarantee yet and is dropped
/// outright, never parsed. Among the fully newline-terminated lines, the
/// first one that fails to parse stops the read -- a conservative,
/// defensive backstop for a ledger that should never actually contain a
/// mid-file corruption given the ordering rule above, but "stop, don't
/// guess" is the right response if one is ever found.
fn read_completed_units(path: &Path) -> Result<Vec<usize>, ResumeError> {
    let Some(content) = read_opt(path)? else {
        return Ok(Vec::new());
    };
    let complete_portion = match content.rfind('\n') {
        Some(pos) => &content[..pos],
        None => "",
    };
    let mut completed = Vec::new();
    for line in complete_portion.split('\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<UnitRecord>(line) {
            Ok(record) => completed.push(record.unit),
            Err(_) => break,
        }
    }
    Ok(completed)
}

/// `alignment.json`'s on-disk shape: `StackAlignment` with every `f64`
/// carried as its `f64::to_bits()` `u64`, exactly the encoding the stage
/// cache uses for the same reason -- `CachedFrameAlignment` is the one
/// shared encoder, reused rather than copied.
#[derive(Debug, Serialize, Deserialize)]
struct LedgerAlignment {
    reference: usize,
    policy: ReferencePolicy,
    frames: Vec<CachedFrameAlignment>,
}

/// Reads `alignment.json` back, if present. Absence is `Ok(None)`, never an
/// error -- a ledger whose run had not yet reached the fuse stage has no
/// recorded alignment at all.
///
/// `pub`: also the decoder `crates/cli/src/stack.rs`'s
/// `peek_resumable_crop_and_alignment` uses to peek a not-yet-`begin`'d
/// ledger's `alignment.json` -- the one place outside this module that
/// reads the file, and it must decode the same
/// `LedgerAlignment`/[`CachedFrameAlignment`] bit-exact shape this
/// function does rather than parsing `StackAlignment` straight off the
/// wire, which does not match the on-disk field names.
pub fn read_alignment(path: &Path) -> Result<Option<StackAlignment>, ResumeError> {
    let Some(text) = read_opt(path)? else {
        return Ok(None);
    };
    let encoded: LedgerAlignment =
        serde_json::from_str(&text).map_err(|source| ResumeError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(Some(StackAlignment {
        reference: encoded.reference,
        policy: encoded.policy,
        frames: encoded
            .frames
            .into_iter()
            .map(CachedFrameAlignment::into_frame_alignment)
            .collect(),
    }))
}

/// Writes `value` as JSON to `path` atomically: a temp file in the same
/// directory, `sync_all`, then `fs::rename` -- so a reader can never observe
/// a partially written `job.json` or `alignment.json`. This, plus
/// [`Ledger::record_unit`]'s per-unit path having **no** sync at all, is the
/// whole of this module's crash-safety budget: write-once files are made
/// durable before they are visible; the append-only log relies on ordering
/// plus [`crate::io::TiffWriter::holds_rows`] instead.
///
/// `pub(super)`: shared with [`super::stage_cache`], the stage cache
/// module, rather than copied there.
pub(super) fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ResumeError> {
    let json = serde_json::to_vec(value).map_err(|source| ResumeError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let tmp_path = path.with_extension("tmp");
    let mut tmp = File::create(&tmp_path).map_err(|source| ResumeError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    tmp.write_all(&json).map_err(|source| ResumeError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    tmp.sync_all().map_err(|source| ResumeError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    drop(tmp);
    fs::rename(&tmp_path, path).map_err(|source| ResumeError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Compares one identity field of a candidate ledger's [`JobRecord`] against
/// the freshly computed one, returning a [`Resumable::Mismatch`] on the
/// first disagreement -- the same "refuse, name what changed" posture as
/// [`crate::job::fingerprint::StackFingerprint::compare`], for the fields a
/// per-frame fingerprint cannot see (algorithm, parameters, tile geometry).
macro_rules! check_identity {
    ($path:expr, $field:literal, $expected:expr, $found:expr) => {
        if $expected != $found {
            return Ok(Resumable::Mismatch(FingerprintMismatch {
                frame: 0,
                path: $path.clone(),
                field: $field,
                expected: format!("{:?}", $expected),
                found: format!("{:?}", $found),
            }));
        }
    };
}

/// The durable resume store under `<scratch_base>/resume/`.
pub struct ResumeStore {
    root: PathBuf,
}

impl ResumeStore {
    /// Creates `<scratch_base>/resume/` (and its ancestors) if it does not
    /// exist yet -- the same posture as `ScratchDir::create_in`.
    pub fn open(scratch_base: &Path) -> Result<Self, ResumeError> {
        let root = scratch_base.join("resume");
        fs::create_dir_all(&root).map_err(|source| ResumeError::Io {
            path: root.clone(),
            source,
        })?;
        Ok(ResumeStore { root })
    }

    /// Starts a fresh run: a new uniquely named child directory, `job.json`
    /// written atomically -- the only sync-heavy write in the whole
    /// design, besides [`Ledger::record_alignment`] -- and an empty
    /// `units.log` opened for append.
    pub fn begin(&self, record: &JobRecord) -> Result<Ledger, ResumeError> {
        // A resume directory is durable state: no guard removes it.
        let dir = unique_child_dir(&self.root)
            .map_err(|(path, source)| ResumeError::Io { path, source })?;
        write_atomic(&dir.join("job.json"), record)?;
        Ledger::open(dir)
    }

    /// Looks for a resumable ledger matching `record`'s identity. See this
    /// module's own docs, "The resume path, end to end".
    ///
    /// **No key string, no hash**: lists `<scratch_base>/resume/*/job.json`,
    /// parses each, skips any whose `composite_output` no longer exists (a
    /// stale ledger from an already-finished, deleted run), and compares the
    /// first remaining candidate's identity against
    /// `record` field by field.
    pub fn find(&self, record: &JobRecord) -> Result<Resumable, ResumeError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Resumable::None);
            }
            Err(source) => {
                return Err(ResumeError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        };

        for entry in entries {
            let entry = entry.map_err(|source| ResumeError::Io {
                path: self.root.clone(),
                source,
            })?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let job_json = dir.join("job.json");
            let text = match fs::read_to_string(&job_json) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let candidate: JobRecord = match serde_json::from_str(&text) {
                Ok(candidate) => candidate,
                Err(_) => continue,
            };
            if !candidate.composite_output.exists() {
                continue;
            }

            if candidate.fingerprint.frames.len() != record.fingerprint.frames.len() {
                let counts = FrameCount {
                    expected: candidate.fingerprint.frames.len(),
                    found: record.fingerprint.frames.len(),
                };
                return Ok(Resumable::Mismatch(FingerprintMismatch {
                    frame: 0,
                    path: candidate.composite_output.clone(),
                    field: "frame_count",
                    expected: counts.expected.to_string(),
                    found: counts.found.to_string(),
                }));
            }
            if let Err(mismatch) = candidate.fingerprint.compare(&record.fingerprint) {
                return Ok(Resumable::Mismatch(mismatch));
            }

            check_identity!(
                candidate.composite_output,
                "tile_size",
                candidate.tile_size,
                record.tile_size
            );
            check_identity!(
                candidate.composite_output,
                "levels",
                candidate.levels,
                record.levels
            );
            check_identity!(
                candidate.composite_output,
                "stack_algorithm",
                candidate.stack_algorithm,
                record.stack_algorithm
            );
            check_identity!(
                candidate.composite_output,
                "stack_params",
                candidate.stack_params,
                record.stack_params
            );
            check_identity!(
                candidate.composite_output,
                "backend",
                candidate.backend,
                record.backend
            );
            check_identity!(
                candidate.composite_output,
                "crop",
                candidate.crop,
                record.crop
            );
            check_identity!(
                candidate.composite_output,
                "align",
                candidate.align,
                record.align
            );
            check_identity!(
                candidate.composite_output,
                "equalization",
                candidate.equalization,
                record.equalization
            );

            let ledger = Ledger::open(dir)?;
            return Ok(Resumable::Ledger(ledger));
        }

        Ok(Resumable::None)
    }
}

/// Decorates a real [`TileConsumer`], recording each unit's completion into
/// a [`Ledger`] only after the inner consumer's write has already returned,
/// which is what keeps every ledger entry in the ordering `record_unit`
/// requires.
pub struct RecordingConsumer<'a> {
    pub inner: &'a dyn TileConsumer,
    pub ledger: &'a Ledger,
}

impl TileConsumer for RecordingConsumer<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        self.inner.consume(job)?;
        self.ledger
            .record_unit(job.tile().index())
            .map_err(|source| AlgorithmError::Failed {
                algorithm: "resume-ledger".to_string(),
                detail: source.to_string(),
            })
    }

    fn abort(&self) {
        self.inner.abort();
    }

    /// Forwarded, never left at the trait's `false` default: this decorator
    /// is transparent, and a GPU consumer wrapped in it would otherwise make
    /// the scheduler read every frame-tile on the host for a consumer that
    /// does not use them.
    fn supplies_own_pixels(&self) -> bool {
        self.inner.supplies_own_pixels()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::fingerprint::StackFingerprint;
    use std::path::PathBuf as StdPathBuf;
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn sample_record(scratch: &Path, output_name: &str) -> JobRecord {
        let output = scratch.join(output_name);
        // The output file must exist for `find` to consider a ledger
        // resumable at all (the stale-ledger guard).
        fs::write(&output, b"x").unwrap();
        JobRecord {
            fingerprint: StackFingerprint { frames: Vec::new() },
            width: 100,
            height: 100,
            samples_per_pixel: 3,
            frame_count: 4,
            tile_size: 200,
            levels: 3,
            stack_algorithm: "laplacian".to_string(),
            stack_params: crate::params::ParamValues::new(),
            backend: "cpu".to_string(),
            align: None,
            crop: None,
            equalization: None,
            composite_output: output,
            zmap_output: None,
        }
    }

    fn tmp_dir(label: &str) -> StdPathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ff-ledger-test-{}-{label}-{}",
            process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A consumer that supplies its own pixels and records nothing else --
    /// stands in for `crate::fuse::BackendFusionConsumer`, which lives above
    /// this module and needs a GPU backend to build.
    struct PixelSupplier;

    impl TileConsumer for PixelSupplier {
        fn consume(&self, _job: &TileJob<'_>) -> Result<(), AlgorithmError> {
            Ok(())
        }

        fn supplies_own_pixels(&self) -> bool {
            true
        }
    }

    #[test]
    fn recording_consumer_forwards_supplies_own_pixels() {
        let scratch = tmp_dir("supplies-own-pixels");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        let ledger = store.begin(&record).unwrap();

        let inner = PixelSupplier;
        let recording = RecordingConsumer {
            inner: &inner,
            ledger: &ledger,
        };
        assert!(
            recording.supplies_own_pixels(),
            "wrapping a pixel-supplying consumer in the ledger decorator must not \
             put the scheduler's host read loop back"
        );

        drop(ledger);
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_changed_backend_is_refused() {
        let scratch = tmp_dir("changed-backend");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        let ledger = store.begin(&record).unwrap();
        ledger.record_unit(0).unwrap();
        drop(ledger);

        let mut moved = record.clone();
        moved.backend = "gpu:some-adapter".to_string();
        match store.find(&moved).unwrap() {
            Resumable::Mismatch(mismatch) => {
                let text = mismatch.to_string();
                assert!(text.contains("backend changed"), "{text}");
                assert!(text.contains("gpu:some-adapter"), "{text}");
                assert!(text.contains("cpu"), "{text}");
            }
            other => panic!("expected a backend mismatch, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn two_begins_under_one_store_get_distinct_directories() {
        let scratch = tmp_dir("distinct-dirs");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        let first = store.begin(&record).unwrap();
        let second = store.begin(&record).unwrap();
        assert_ne!(first.dir(), second.dir());
        assert!(first.dir().join("job.json").is_file());
        assert!(second.dir().join("job.json").is_file());
        drop((first, second));
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn begin_then_find_round_trips_the_ledger() {
        let scratch = tmp_dir("round-trip");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");

        let ledger = store.begin(&record).unwrap();
        ledger.record_unit(0).unwrap();
        ledger.record_unit(1).unwrap();
        drop(ledger);

        match store.find(&record).unwrap() {
            Resumable::Ledger(ledger) => {
                assert_eq!(ledger.completed(), &[0, 1]);
            }
            other => panic!("expected Resumable::Ledger, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_torn_trailing_record_is_read_as_incomplete() {
        let scratch = tmp_dir("torn");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        let ledger = store.begin(&record).unwrap();
        ledger.record_unit(0).unwrap();
        // Simulate a crash mid-write of the *next* line: append a
        // non-newline-terminated, syntactically-broken tail directly.
        {
            let mut file = ledger.units_log.lock().unwrap();
            file.write_all(b"{\"unit\":1").unwrap();
        }
        drop(ledger);

        match store.find(&record).unwrap() {
            Resumable::Ledger(ledger) => {
                assert_eq!(
                    ledger.completed(),
                    &[0],
                    "a torn trailing record must not count as complete"
                );
            }
            other => panic!("expected Resumable::Ledger, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn empty_store_is_resumable_none() {
        let scratch = tmp_dir("empty");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        match store.find(&record).unwrap() {
            Resumable::None => {}
            other => panic!("expected Resumable::None, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn discard_removes_the_directory() {
        let scratch = tmp_dir("discard");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        let ledger = store.begin(&record).unwrap();
        let dir = ledger.dir().to_path_buf();
        assert!(dir.is_dir());
        ledger.discard().unwrap();
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&scratch);
    }

    /// The link targets, under `/proc/self/fd`, of this process's open
    /// descriptors that resolve inside `dir`.
    #[cfg(target_os = "linux")]
    fn open_fd_targets_under(dir: &Path) -> Vec<StdPathBuf> {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|target| target.starts_with(dir))
            .collect()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn discard_closes_the_log_before_removing() {
        let scratch = tmp_dir("closes-log-first");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");

        let assert_close_drops_the_handle = |ledger: Ledger| {
            let dir = ledger.dir().to_path_buf();
            assert!(
                !open_fd_targets_under(&dir).is_empty(),
                "expected the units.log handle to be visible under /proc/self/fd before close()"
            );
            let closed_dir = ledger.close();
            assert_eq!(closed_dir, dir);
            assert!(
                open_fd_targets_under(&dir).is_empty(),
                "units.log handle must be closed before its directory is removed"
            );
            fs::remove_dir_all(&dir).unwrap();
        };

        // A freshly begun ledger with a recorded unit.
        let ledger = store.begin(&record).unwrap();
        ledger.record_unit(0).unwrap();
        assert_close_drops_the_handle(ledger);

        // A ledger reopened through `find`, after a unit was recorded and
        // the first handle onto it dropped.
        let ledger = store.begin(&record).unwrap();
        ledger.record_unit(0).unwrap();
        drop(ledger);
        let ledger = match store.find(&record).unwrap() {
            Resumable::Ledger(ledger) => ledger,
            other => panic!("expected Resumable::Ledger, got {other:?}"),
        };
        assert_close_drops_the_handle(ledger);

        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn discard_reports_the_directory_it_could_not_remove() {
        let scratch = tmp_dir("discard-gone");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        let ledger = store.begin(&record).unwrap();
        let dir = ledger.dir().to_path_buf();
        // Removed out from under the ledger: `remove_dir_all` must still
        // name this directory in its error, not panic or report success.
        fs::remove_dir_all(&dir).unwrap();
        match ledger.discard() {
            Err(ResumeError::Io { path, .. }) => assert_eq!(path, dir),
            other => panic!("expected ResumeError::Io naming {dir:?}, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn discard_removes_a_resumed_ledger_with_alignment() {
        let scratch = tmp_dir("discard-resumed");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");

        let fresh = store.begin(&record).unwrap();
        fresh.record_unit(0).unwrap();
        let alignment = StackAlignment {
            reference: 0,
            policy: crate::align::ReferencePolicy::Anchor,
            frames: vec![crate::align::FrameAlignment {
                frame: 0,
                transform: crate::align::Transform::identity(),
                status: crate::algorithm::AlignStatus::Converged,
                iterations: 0,
                residual: 1.0,
                fallback: false,
            }],
        };
        fresh.record_alignment(&alignment).unwrap();
        drop(fresh);

        let resumed = match store.find(&record).unwrap() {
            Resumable::Ledger(ledger) => ledger,
            other => panic!("expected Resumable::Ledger, got {other:?}"),
        };
        let dir = resumed.dir().to_path_buf();
        assert!(dir.is_dir());
        resumed.discard().unwrap();
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(&scratch);
    }

    #[cfg(unix)]
    #[test]
    fn discard_reports_a_removal_it_cannot_complete() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = tmp_dir("discard-unremovable");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        let ledger = store.begin(&record).unwrap();
        let dir = ledger.dir().to_path_buf();

        let foreign = dir.join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("stray"), b"x").unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o555)).unwrap();

        // Root bypasses directory permissions, so the removal cannot fail there.
        let probe = foreign.join("probe");
        if fs::write(&probe, b"x").is_ok() {
            let _ = fs::remove_file(&probe);
            fs::set_permissions(&foreign, fs::Permissions::from_mode(0o755)).unwrap();
            let _ = fs::remove_dir_all(&scratch);
            eprintln!("skipping: running as root, permissions do not block removal");
            return;
        }

        match ledger.discard() {
            Err(ResumeError::Io { path, .. }) => assert_eq!(path, dir),
            other => panic!("expected ResumeError::Io naming {dir:?}, got {other:?}"),
        }
        assert!(
            dir.exists(),
            "a failed discard must leave the directory in place"
        );

        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn alignment_round_trips_through_the_ledger() {
        let scratch = tmp_dir("alignment");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        let ledger = store.begin(&record).unwrap();
        assert!(ledger.alignment().is_none());

        let alignment = StackAlignment {
            reference: 0,
            policy: crate::align::ReferencePolicy::Anchor,
            frames: vec![crate::align::FrameAlignment {
                frame: 0,
                transform: crate::align::Transform::identity(),
                status: crate::algorithm::AlignStatus::Converged,
                iterations: 0,
                residual: 1.0,
                fallback: false,
            }],
        };
        ledger.record_alignment(&alignment).unwrap();
        drop(ledger);

        match store.find(&record).unwrap() {
            Resumable::Ledger(ledger) => {
                assert_eq!(ledger.alignment(), Some(&alignment));
            }
            other => panic!("expected Resumable::Ledger, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn changed_algorithm_params_are_refused() {
        let scratch = tmp_dir("changed-params");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        store.begin(&record).unwrap();

        let mut changed = record.clone();
        changed
            .stack_params
            .insert("levels".to_string(), crate::params::ParamValue::Int(9));

        match store.find(&changed).unwrap() {
            Resumable::Mismatch(mismatch) => assert_eq!(mismatch.field, "stack_params"),
            other => panic!("expected Resumable::Mismatch, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn changed_tile_geometry_is_refused() {
        let scratch = tmp_dir("changed-geometry");
        let store = ResumeStore::open(&scratch).unwrap();
        let record = sample_record(&scratch, "out.tif");
        store.begin(&record).unwrap();

        let mut changed = record.clone();
        changed.tile_size = 999;

        match store.find(&changed).unwrap() {
            Resumable::Mismatch(mismatch) => assert_eq!(mismatch.field, "tile_size"),
            other => panic!("expected Resumable::Mismatch, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&scratch);
    }
}
