// SPDX-License-Identifier: GPL-3.0-or-later

//! The one file every persisted GUI setting saves through:
//! `<config_dir>/settings.toml`, one top-level table per concern
//! (`[recent]`, `[input]`, `[export]`, `[performance]`). Each concern
//! (`crate::recent`, `crate::input_config`) reads its own key out of the
//! whole file and, on save, replaces only that key -- a hand-edited sibling
//! table, comments included, is never touched. No `use slint::` anywhere in
//! this file (`input_modules_are_slint_free`).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table};

pub const FILE_NAME: &str = "settings.toml";

/// The file's path and raw text. `Ok(None)` means the file does not exist
/// yet -- the normal first run, not an error. Any other read failure
/// (permissions, non-UTF-8 bytes, a directory where the file should be) is
/// `Err`: it must never be read as "absent", which would overwrite bytes
/// that are not safe to touch.
pub fn read(config_dir: &Path) -> (PathBuf, std::io::Result<Option<String>>) {
    let path = config_dir.join(FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(src) => Ok(Some(src)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    };
    (path, text)
}

/// Turns a `Serialize` value into a `toml_edit::Table` by way of `toml`'s
/// own serialisation, so a caller holding a serde type needs no dependency
/// on `toml_edit`'s serde feature.
pub fn to_table<T: Serialize>(value: &T) -> Table {
    let text = toml::to_string(value).expect("value always serialises to TOML");
    let mut table = text
        .parse::<DocumentMut>()
        .expect("toml::to_string always emits valid TOML")
        .into_table();
    clear_positions(&mut table);
    table
}

/// Clears `position` on `table` and everything nested under it.
/// `toml_edit` prints top-level tables in `position` order, so a table
/// lifted whole from another parsed document (a freshly serialised value)
/// carries that document's positions and would interleave with the host
/// document's tables unless cleared first.
fn clear_positions(table: &mut Table) {
    table.set_position(None);
    for (_, item) in table.iter_mut() {
        if let Some(t) = item.as_table_mut() {
            clear_positions(t);
        } else if let Some(aot) = item.as_array_of_tables_mut() {
            for t in aot.iter_mut() {
                clear_positions(t);
            }
        }
    }
}

/// Every way writing one table back to `settings.toml` can fail.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("{path}: could not create the config directory: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: could not read the existing file, left untouched: {source}")]
    CouldNotRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: not valid TOML, left untouched: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("{path}: could not write: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Replaces `settings.toml`'s `[<key>]` table with `table` and writes the
/// file back, creating `config_dir` if needed. Every other top-level
/// table, hand edits included, is left exactly as it was. A file that does
/// not parse as TOML at all, or cannot be read (permissions, non-UTF-8
/// bytes, a directory in its place), is refused rather than overwritten.
pub fn write_table(config_dir: &Path, key: &str, mut table: Table) -> Result<PathBuf, WriteError> {
    fs::create_dir_all(config_dir).map_err(|source| WriteError::CreateDir {
        path: config_dir.to_path_buf(),
        source,
    })?;
    let path = config_dir.join(FILE_NAME);
    let mut doc = match fs::read_to_string(&path) {
        Ok(src) => src
            .parse::<DocumentMut>()
            .map_err(|source| WriteError::Parse {
                path: path.clone(),
                source,
            })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(source) => {
            return Err(WriteError::CouldNotRead {
                path: path.clone(),
                source,
            });
        }
    };
    clear_positions(&mut table);
    doc.insert(key, Item::Table(table));
    fs::write(&path, doc.to_string()).map_err(|source| WriteError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}
