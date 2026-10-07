// SPDX-License-Identifier: GPL-3.0-or-later

//! The project file: a fixed header, records appended one after another and
//! a footer that indexes every live record.
//!
//! ```text
//! header | record ... | footer | record ... | footer
//! ```
//!
//! A record is a named run of bytes (`manifest.json`,
//! `checkpoints/<id>.json`, `blobs/<hash>.zst`). The footer, a JSON map
//! `{name: [offset, length]}` and a checksummed trailer, is the commit point:
//! records are synced before the footer that names them. Opening reads the
//! footer the trailer at the end names, else scans backwards for the previous
//! valid one; bytes after it belong to an interrupted save.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::error::ProjectError;

const MAGIC: [u8; 8] = *b"FFPROJ\0\n";
const TRAILER_MAGIC: [u8; 8] = *b"FFPEND\0\n";

/// Bytes in the file header.
pub const HEADER_LEN: u64 = MAGIC.len() as u64;

/// Bytes in a footer's fixed tail: the JSON's blake3, its length and the
/// trailer magic.
pub const TRAILER_LEN: u64 = 32 + 8 + TRAILER_MAGIC.len() as u64;

/// How far one step of the backward footer scan reads.
const SCAN_CHUNK: u64 = 1 << 20;

/// `<path>.partial`: the suffix appended to the file name, in `path`'s own
/// directory. Shared with `crates/cli`, whose `export` writes the same
/// `.partial`-then-rename discipline for the image files it produces from a
/// project.
pub fn partial_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    path.with_file_name(name)
}

/// A `<final_path>.partial` file beside `final_path`: dropping the guard
/// removes it, [`Self::commit`] renames it onto the final path. The caller
/// creates the file at [`Self::path`] and closes it before the guard drops.
#[derive(Debug)]
pub struct PartialFile {
    partial: PathBuf,
    final_path: PathBuf,
}

impl PartialFile {
    /// Guards `<final_path>.partial`; nothing is created until the caller
    /// writes to [`Self::path`].
    pub fn new(final_path: &Path) -> Self {
        PartialFile {
            partial: partial_path(final_path),
            final_path: final_path.to_path_buf(),
        }
    }

    /// Where to write.
    pub fn path(&self) -> &Path {
        &self.partial
    }

    /// Renames the partial onto the final path.
    pub fn commit(self) -> std::io::Result<()> {
        std::fs::rename(&self.partial, &self.final_path)
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.partial);
    }
}

/// The live records of one committed state: where each name's bytes sit.
#[derive(Debug, Clone)]
pub struct Index {
    entries: BTreeMap<String, (u64, u64)>,
    /// One past the footer: the committed end of the file.
    end: u64,
    /// Bytes the footer takes, tail included.
    footer_len: u64,
}

impl Index {
    fn empty() -> Self {
        Index {
            entries: BTreeMap::new(),
            end: HEADER_LEN,
            footer_len: 0,
        }
    }

    /// Reads the last committed state of the project at `path`.
    ///
    /// Empty, shorter than the header or a foreign magic is
    /// [`ProjectError::NotAProject`]; a header with no footer that validates
    /// is [`ProjectError::Truncated`].
    pub fn read(path: &Path) -> Result<Self, ProjectError> {
        let file = File::open(path).map_err(|source| ProjectError::io(path, source))?;
        Self::read_from(&file, path)
    }

    pub(super) fn read_from(mut file: &File, path: &Path) -> Result<Self, ProjectError> {
        let io = |source| ProjectError::io(path, source);
        let not_a_project = || ProjectError::NotAProject {
            path: path.to_path_buf(),
        };
        let len = file.metadata().map_err(io)?.len();
        if len < HEADER_LEN {
            return Err(not_a_project());
        }
        let mut header = [0u8; MAGIC.len()];
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        file.read_exact(&mut header).map_err(io)?;
        if header != MAGIC {
            return Err(not_a_project());
        }
        if let Some(index) = footer_ending_at(file, len, path)? {
            return Ok(index);
        }
        scan_back(file, len, path)?.ok_or_else(|| ProjectError::Truncated {
            path: path.to_path_buf(),
        })
    }

    /// `(offset, length)` of the record `name` names.
    pub fn get(&self, name: &str) -> Option<(u64, u64)> {
        self.entries.get(name).copied()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Every live record name, in name order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Every live record as `(name, offset, length)`, in name order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, u64, u64)> {
        self.entries
            .iter()
            .map(|(name, &(offset, len))| (name.as_str(), offset, len))
    }

    /// Where the committed state ends: one past its footer.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Where the footer of this state starts.
    pub fn footer_start(&self) -> u64 {
        self.end - self.footer_len
    }

    /// Bytes before the committed end that this state does not use: records
    /// it no longer lists, replaced records and every earlier footer.
    pub fn dead_bytes(&self) -> u64 {
        let live: u64 = self.entries.values().map(|&(_, len)| len).sum();
        self.end.saturating_sub(HEADER_LEN + self.footer_len + live)
    }

    /// Whether more than half of the committed file is dead bytes.
    pub fn mostly_dead(&self) -> bool {
        self.dead_bytes() * 2 > self.end
    }

    /// Reads one record whole from the file at `path`.
    pub fn read_entry(&self, path: &Path, name: &str) -> Result<Vec<u8>, ProjectError> {
        let file = File::open(path).map_err(|source| ProjectError::io(path, source))?;
        let mut bytes = Vec::new();
        record_reader(&file, self, path, name)?
            .read_to_end(&mut bytes)
            .map_err(|source| ProjectError::io(path, source))?;
        Ok(bytes)
    }
}

/// A reader over exactly the bytes of record `name`: the bound keeps a zstd
/// decoder, which runs through consecutive frames, from reading into the
/// records behind it.
pub(super) fn record_reader<'f>(
    mut file: &'f File,
    index: &Index,
    path: &Path,
    name: &str,
) -> Result<std::io::Take<&'f File>, ProjectError> {
    let (offset, len) = index.get(name).ok_or_else(|| ProjectError::MissingEntry {
        path: path.to_path_buf(),
        entry: name.to_string(),
    })?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|source| ProjectError::io(path, source))?;
    Ok(file.take(len))
}

/// The footer whose trailer ends at `end`, when every check on it passes.
fn footer_ending_at(mut file: &File, end: u64, path: &Path) -> Result<Option<Index>, ProjectError> {
    if end < HEADER_LEN + TRAILER_LEN {
        return Ok(None);
    }
    let io = |source| ProjectError::io(path, source);
    let mut trailer = [0u8; TRAILER_LEN as usize];
    file.seek(SeekFrom::Start(end - TRAILER_LEN)).map_err(io)?;
    file.read_exact(&mut trailer).map_err(io)?;
    if trailer[40..] != TRAILER_MAGIC {
        return Ok(None);
    }
    let json_len = u64::from_le_bytes(trailer[32..40].try_into().expect("eight bytes"));
    let Some(footer_start) = (end - TRAILER_LEN)
        .checked_sub(json_len)
        .filter(|&start| start >= HEADER_LEN)
    else {
        return Ok(None);
    };
    let mut json = Vec::new();
    file.seek(SeekFrom::Start(footer_start)).map_err(io)?;
    file.take(json_len).read_to_end(&mut json).map_err(io)?;
    if json.len() as u64 != json_len || blake3::hash(&json).as_bytes() != &trailer[..32] {
        return Ok(None);
    }
    let Ok(entries) = serde_json::from_slice::<BTreeMap<String, (u64, u64)>>(&json) else {
        return Ok(None);
    };
    let inside = |&(offset, len): &(u64, u64)| {
        offset >= HEADER_LEN
            && offset
                .checked_add(len)
                .is_some_and(|stop| stop <= footer_start)
    };
    if !entries.values().all(inside) {
        return Ok(None);
    }
    Ok(Some(Index {
        entries,
        end,
        footer_len: json_len + TRAILER_LEN,
    }))
}

/// The newest valid footer before the end of the file, found by looking for
/// trailer magics backwards. The one ending at `len` was already tried.
fn scan_back(mut file: &File, len: u64, path: &Path) -> Result<Option<Index>, ProjectError> {
    let io = |source| ProjectError::io(path, source);
    let magic = TRAILER_MAGIC.len();
    let mut buf = Vec::new();
    let mut hi = len;
    loop {
        let lo = hi.saturating_sub(SCAN_CHUNK).max(HEADER_LEN);
        if ((hi - lo) as usize) < magic {
            return Ok(None);
        }
        buf.resize((hi - lo) as usize, 0);
        file.seek(SeekFrom::Start(lo)).map_err(io)?;
        file.read_exact(&mut buf).map_err(io)?;
        for at in (0..=buf.len() - magic).rev() {
            if buf[at..at + magic] != TRAILER_MAGIC {
                continue;
            }
            let end = lo + (at + magic) as u64;
            if end < len
                && let Some(index) = footer_ending_at(file, end, path)?
            {
                return Ok(Some(index));
            }
        }
        if lo == HEADER_LEN {
            return Ok(None);
        }
        // The next chunk overlaps by one byte less than a magic, so one that
        // straddles the boundary is still seen whole.
        hi = lo + magic as u64 - 1;
    }
}

/// Writes records into one project file. [`Self::create`] makes a new file
/// that appears at its path only once [`Self::finish`] has committed it;
/// [`Self::append`] adds to an existing one. A writer dropped before `finish`
/// leaves the file as it was.
pub struct ContainerWriter {
    // Taken by `finish`, so the handle is closed before the rename.
    file: Option<File>,
    path: PathBuf,
    /// Where an unfinished append cuts the file back to.
    committed_end: u64,
    position: u64,
    index: Index,
    // Declared after `file`: the handle closes before the guard removes the file.
    partial: Option<PartialFile>,
    finished: bool,
}

impl ContainerWriter {
    /// Starts a new project at `<final_path>.partial`. Nothing appears at
    /// `final_path` until [`Self::finish`] succeeds.
    pub fn create(final_path: &Path) -> Result<Self, ProjectError> {
        let at = partial_path(final_path);
        let mut file = File::create(&at).map_err(|source| ProjectError::io(&at, source))?;
        let partial = PartialFile::new(final_path);
        file.write_all(&MAGIC)
            .map_err(|source| ProjectError::io(&at, source))?;
        Ok(ContainerWriter {
            file: Some(file),
            path: at,
            committed_end: 0,
            position: HEADER_LEN,
            index: Index::empty(),
            partial: Some(partial),
            finished: false,
        })
    }

    /// Continues the project at `path` after its committed state `committed`,
    /// cutting off any torn tail an interrupted save left behind.
    pub fn append(path: &Path, committed: &Index) -> Result<Self, ProjectError> {
        let io = |source| ProjectError::io(path, source);
        let mut file = OpenOptions::new().write(true).open(path).map_err(io)?;
        if file.metadata().map_err(io)?.len() < committed.end {
            return Err(ProjectError::Truncated {
                path: path.to_path_buf(),
            });
        }
        file.set_len(committed.end).map_err(io)?;
        file.seek(SeekFrom::Start(committed.end)).map_err(io)?;
        Ok(ContainerWriter {
            file: Some(file),
            path: path.to_path_buf(),
            committed_end: committed.end,
            position: committed.end,
            index: committed.clone(),
            partial: None,
            finished: false,
        })
    }

    /// The records this writer has put or kept so far. `end` and the dead
    /// byte count are those of the state it started from.
    pub fn index(&self) -> &Index {
        &self.index
    }

    fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("the file is open until finish")
    }

    /// Appends `source` as the record `name`, replacing an earlier record of
    /// that name. A `blobs/` name already indexed is skipped: its name is its
    /// content hash.
    pub fn put(&mut self, name: &str, source: &mut impl Read) -> Result<(), ProjectError> {
        if name.starts_with("blobs/") && self.index.contains(name) {
            return Ok(());
        }
        match std::io::copy(source, self.file()) {
            Ok(len) => {
                self.index
                    .entries
                    .insert(name.to_string(), (self.position, len));
                self.position += len;
                Ok(())
            }
            Err(source) => {
                let at = self.position;
                let file = self.file();
                let _ = file.set_len(at);
                let _ = file.seek(SeekFrom::Start(at));
                Err(ProjectError::io(&self.path, source))
            }
        }
    }

    /// [`Self::put`] for bytes already in memory.
    pub fn put_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<(), ProjectError> {
        self.put(name, &mut &*bytes)
    }

    /// Stops listing `name`; its bytes stay in the file as dead data.
    pub fn drop_entry(&mut self, name: &str) {
        self.index.entries.remove(name);
    }

    /// Syncs the records, appends the footer that lists them and syncs again.
    /// A new file is renamed onto its path last. Returns the committed state.
    pub fn finish(mut self) -> Result<Index, ProjectError> {
        let path = self.path.clone();
        let io = |source| ProjectError::io(&path, source);
        self.file().sync_data().map_err(io)?;
        let json = serde_json::to_vec(&self.index.entries)
            .map_err(|err| ProjectError::container(&path, err))?;
        let json_len = json.len() as u64;
        let mut footer = Vec::with_capacity(json.len() + TRAILER_LEN as usize);
        footer.extend_from_slice(&json);
        footer.extend_from_slice(blake3::hash(&json).as_bytes());
        footer.extend_from_slice(&json_len.to_le_bytes());
        footer.extend_from_slice(&TRAILER_MAGIC);
        let file = self.file();
        file.write_all(&footer).map_err(io)?;
        file.sync_data().map_err(io)?;
        self.index.end = self.position + footer.len() as u64;
        self.index.footer_len = footer.len() as u64;
        self.file = None;
        if let Some(partial) = self.partial.take() {
            partial.commit().map_err(io)?;
        }
        self.finished = true;
        Ok(self.index.clone())
    }
}

/// Rewrites the project at `path` to its live records only, through a
/// synced sibling file renamed over it, and returns the bytes reclaimed. A
/// project with no dead bytes and no torn tail is left untouched (0).
/// Record names, hence checkpoint ids and blob hashes, do not change.
pub fn compact(path: &Path) -> Result<u64, ProjectError> {
    compact_from(path, &Index::read(path)?)
}

/// [`compact`] when more than half of the committed file is dead bytes.
pub fn compact_if_mostly_dead(path: &Path) -> Result<Option<u64>, ProjectError> {
    let index = Index::read(path)?;
    if !index.mostly_dead() {
        return Ok(None);
    }
    compact_from(path, &index).map(Some)
}

fn compact_from(path: &Path, index: &Index) -> Result<u64, ProjectError> {
    let io = |source| ProjectError::io(path, source);
    let old_len = std::fs::metadata(path).map_err(io)?.len();
    if index.dead_bytes() == 0 && old_len == index.end() {
        return Ok(0);
    }
    let source = File::open(path).map_err(io)?;
    let mut writer = ContainerWriter::create(path)?;
    for name in index.names() {
        writer.put(name, &mut record_reader(&source, index, path, name)?)?;
    }
    drop(source);
    let compacted = writer.finish()?;
    Ok(old_len.saturating_sub(compacted.end()))
}

impl Drop for ContainerWriter {
    fn drop(&mut self) {
        if !self.finished
            && self.partial.is_none()
            && let Some(file) = &self.file
        {
            let _ = file.set_len(self.committed_end);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_path_appends_the_suffix_to_the_file_name() {
        assert_eq!(
            partial_path(Path::new("/tmp/demo.ffp")),
            Path::new("/tmp/demo.ffp.partial")
        );
    }

    #[test]
    fn partial_file_drop_removes_the_partial_and_leaves_no_final() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.tar.tif");
        let guard = PartialFile::new(&final_path);
        assert_eq!(guard.path(), dir.path().join("out.tar.tif.partial"));
        std::fs::write(guard.path(), b"half a file").unwrap();
        drop(guard);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn container_writer_create_failure_leaves_a_preexisting_partial_alone() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("demo.ffp");
        let partial = partial_path(&final_path);
        std::fs::write(&partial, b"someone else's file").unwrap();
        let mut perms = std::fs::metadata(&partial).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&partial, perms).unwrap();

        assert!(ContainerWriter::create(&final_path).is_err());
        assert!(partial.exists());
    }

    #[test]
    fn partial_file_commit_leaves_the_final_file_and_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.tar.tif");
        let guard = PartialFile::new(&final_path);
        std::fs::write(guard.path(), b"whole file").unwrap();
        guard.commit().unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"whole file");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn created(dir: &Path) -> PathBuf {
        let path = dir.join("demo.ffp");
        let mut writer = ContainerWriter::create(&path).unwrap();
        writer
            .put_bytes("manifest.json", b"first manifest")
            .unwrap();
        writer.put_bytes("blobs/aa.zst", b"blob bytes").unwrap();
        writer.finish().unwrap();
        path
    }

    #[test]
    fn create_commits_a_file_that_reads_back_with_no_dead_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = created(dir.path());
        assert!(!partial_path(&path).exists());
        let index = Index::read(&path).unwrap();
        assert_eq!(
            index.names().collect::<Vec<_>>(),
            ["blobs/aa.zst", "manifest.json"]
        );
        assert_eq!(
            index.read_entry(&path, "blobs/aa.zst").unwrap(),
            b"blob bytes"
        );
        assert_eq!(index.dead_bytes(), 0);
        assert_eq!(index.end(), std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn append_replaces_and_drops_names_and_counts_the_old_bytes_as_dead() {
        let dir = tempfile::tempdir().unwrap();
        let path = created(dir.path());
        let before = std::fs::read(&path).unwrap();
        let old = Index::read(&path).unwrap();

        let mut writer = ContainerWriter::append(&path, &old).unwrap();
        writer
            .put_bytes("manifest.json", b"second manifest")
            .unwrap();
        writer.put_bytes("checkpoints/c.json", b"{}").unwrap();
        writer.drop_entry("blobs/aa.zst");
        let new = writer.finish().unwrap();

        let after = std::fs::read(&path).unwrap();
        assert_eq!(&after[..before.len()], &before[..]);
        assert_eq!(
            new.names().collect::<Vec<_>>(),
            ["checkpoints/c.json", "manifest.json"]
        );
        assert_eq!(
            new.read_entry(&path, "manifest.json").unwrap(),
            b"second manifest"
        );
        // The first manifest, the dropped blob and the first footer.
        assert_eq!(
            new.dead_bytes(),
            b"first manifest".len() as u64 + b"blob bytes".len() as u64 + old.footer_len
        );
    }

    #[test]
    fn a_blob_name_already_indexed_is_not_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = created(dir.path());
        let old = Index::read(&path).unwrap();
        let mut writer = ContainerWriter::append(&path, &old).unwrap();
        writer.put_bytes("blobs/aa.zst", b"other bytes").unwrap();
        assert_eq!(writer.index().get("blobs/aa.zst"), old.get("blobs/aa.zst"));
    }

    #[test]
    fn a_torn_trailer_falls_back_to_the_previous_footer_and_the_next_append_cuts_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = created(dir.path());
        let old = Index::read(&path).unwrap();
        let mut writer = ContainerWriter::append(&path, &old).unwrap();
        writer.put_bytes("checkpoints/c.json", b"{}").unwrap();
        writer.finish().unwrap();
        let committed = std::fs::read(&path).unwrap();

        std::fs::write(&path, &committed[..committed.len() - 3]).unwrap();
        let recovered = Index::read(&path).unwrap();
        assert_eq!(recovered.end(), old.end());
        assert!(!recovered.contains("checkpoints/c.json"));

        ContainerWriter::append(&path, &recovered).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), old.end());
    }

    #[test]
    fn a_dropped_append_leaves_the_file_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = created(dir.path());
        let before = std::fs::read(&path).unwrap();
        let old = Index::read(&path).unwrap();
        let mut writer = ContainerWriter::append(&path, &old).unwrap();
        writer.put_bytes("checkpoints/c.json", b"{}").unwrap();
        drop(writer);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn foreign_and_headerless_files_are_not_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.ffp");
        for bytes in [&b""[..], b"FFPR", b"PK\x03\x04 more bytes than a header"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(matches!(
                Index::read(&path),
                Err(ProjectError::NotAProject { .. })
            ));
        }
        std::fs::write(&path, MAGIC).unwrap();
        assert!(matches!(
            Index::read(&path),
            Err(ProjectError::Truncated { .. })
        ));
    }
}
