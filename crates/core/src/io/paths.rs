// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};

use super::TiffError;

pub const TIFF_EXTENSIONS: &[&str] = &["tif", "tiff"];
pub const RAW_EXTENSIONS: &[&str] = &[
    "ari", "arw", "cr2", "cr3", "crm", "crw", "dcr", "dcs", "dng", "erf", "iiq", "kdc", "mef",
    "mos", "mrw", "nef", "nrw", "orf", "ori", "pef", "raf", "raw", "rw2", "rwl", "srw", "3fr",
    "fff", "x3f", "qtk",
];

pub fn frame_paths_for_extensions(
    dir: &Path,
    extensions: &[&str],
) -> Result<Vec<PathBuf>, TiffError> {
    let frames_sub = dir.join("frames");
    let scan_dir = if frames_sub.is_dir() {
        frames_sub
    } else {
        dir.to_path_buf()
    };
    let read_dir = std::fs::read_dir(&scan_dir).map_err(|source| TiffError::Io {
        path: scan_dir.clone(),
        source,
    })?;
    let mut paths = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|source| TiffError::Io {
            path: scan_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.is_file() && has_extension(&path, extensions) {
            paths.push(path);
        }
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if paths.is_empty() {
        return Err(TiffError::NoFrames { path: scan_dir });
    }
    Ok(paths)
}

pub fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extensions
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_extensions_match_rawler() {
        assert_eq!(
            RAW_EXTENSIONS,
            rawler::decoders::supported_extensions()
                .iter()
                .map(|extension| extension.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn frame_paths_for_extensions_prefers_frames_sorts_and_stays_shallow() {
        let dir = tempfile::tempdir().expect("temporary directory");
        std::fs::write(dir.path().join("root.dng"), b"root").expect("root file");
        std::fs::create_dir(dir.path().join("frames")).expect("frames directory");
        std::fs::create_dir(dir.path().join("frames/nested")).expect("nested directory");
        for name in ["z.DNG", "a.TIFF", "ignored.jpg"] {
            std::fs::write(dir.path().join("frames").join(name), b"frame").expect("frame file");
        }
        std::fs::write(dir.path().join("frames/nested/hidden.dng"), b"nested")
            .expect("nested frame");

        let paths =
            frame_paths_for_extensions(dir.path(), &["dng", "tiff"]).expect("recognized frames");
        let names: Vec<_> = paths
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["a.TIFF", "z.DNG"]);
    }
}
