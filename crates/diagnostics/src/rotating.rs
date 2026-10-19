// SPDX-License-Identifier: GPL-3.0-or-later

//! A size-rotating append-only writer for the JSON Lines log file.
//!
//! Rotation happens only between writes, so a rotated file never ends
//! mid-object. Locking recovers from a poisoned mutex instead of panicking,
//! because a panic hook may still need to append its own record after
//! another writer panicked mid-write.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

struct State {
    file: File,
    written: u64,
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
}

/// A cloneable, thread-safe handle to a log file that rotates by size.
#[derive(Clone)]
pub struct RotatingFile {
    state: Arc<Mutex<State>>,
}

impl RotatingFile {
    /// Opens `path` for append, creating it and its parent directory if
    /// needed. Once a write pushes the file past `max_bytes`, the live file
    /// is renamed down to `<stem>.1.<ext>`, existing rotated files shift up
    /// by one, and anything past `keep` rotated files is deleted.
    pub fn open(path: &Path, max_bytes: u64, keep: u32) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                file,
                written,
                path: path.to_path_buf(),
                max_bytes,
                keep,
            })),
        })
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.file.write_all(buf)?;
        state.written += buf.len() as u64;
        if state.written > state.max_bytes {
            rotate(&mut state)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.file.flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = RotatingFile;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// `<stem>.<n>.<ext>` beside `path`, e.g. `focusforge.1.jsonl` for
/// `focusforge.jsonl`.
fn rotated_path(path: &Path, n: u32) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("log");
    match path.extension().and_then(|s| s.to_str()) {
        Some(ext) => path.with_file_name(format!("{stem}.{n}.{ext}")),
        None => path.with_file_name(format!("{stem}.{n}")),
    }
}

fn rotate(state: &mut State) -> io::Result<()> {
    if state.keep == 0 {
        state.file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&state.path)?;
        state.written = 0;
        return Ok(());
    }
    let _ = fs::remove_file(rotated_path(&state.path, state.keep));
    for n in (1..state.keep).rev() {
        let src = rotated_path(&state.path, n);
        if src.exists() {
            fs::rename(&src, rotated_path(&state.path, n + 1))?;
        }
    }
    fs::rename(&state.path, rotated_path(&state.path, 1))?;
    state.file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.path)?;
    state.written = 0;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: usize) -> Vec<u8> {
        // A fixed-width line so a caller can compute exactly how many
        // writes cross a given byte threshold.
        format!("{{\"n\":{n:04}}}\n").into_bytes()
    }

    #[test]
    fn a_write_crossing_the_threshold_leaves_the_previous_content_whole_in_dot_1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focusforge.jsonl");
        let one_line = line(0).len() as u64;
        // Three lines fit under the threshold; the fourth pushes it over,
        // so all four end up rotated together, none of them split.
        let mut writer = RotatingFile::open(&path, one_line * 3, 5).unwrap();

        for n in 0..4 {
            writer.write_all(&line(n)).unwrap();
        }

        let expected: Vec<u8> = (0..4).flat_map(line).collect();
        let rotated = fs::read(rotated_path(&path, 1)).unwrap();
        assert_eq!(rotated, expected);
        let live = fs::read(&path).unwrap();
        assert!(
            live.is_empty(),
            "the freshly reopened live file must start empty"
        );
    }

    #[test]
    fn more_writes_than_keep_leave_exactly_keep_rotated_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focusforge.jsonl");
        let one_line = line(0).len() as u64;
        // Below any single line's size, so every write rotates on its own.
        let mut writer = RotatingFile::open(&path, one_line - 1, 2).unwrap();

        for n in 0..10 {
            writer.write_all(&line(n)).unwrap();
        }

        assert!(rotated_path(&path, 1).is_file());
        assert!(rotated_path(&path, 2).is_file());
        assert!(!rotated_path(&path, 3).exists());
    }

    #[test]
    fn every_rotated_file_ends_in_a_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("focusforge.jsonl");
        let one_line = line(0).len() as u64;
        let mut writer = RotatingFile::open(&path, one_line - 1, 3).unwrap();

        for n in 0..5 {
            writer.write_all(&line(n)).unwrap();
        }

        for n in 1..=3 {
            let rotated = fs::read_to_string(rotated_path(&path, n)).unwrap();
            assert!(
                rotated.ends_with('\n'),
                "rotated file {n} must end in a newline"
            );
        }
    }
}
