// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};

use super::{TiffError, is_frame_path};

pub const TIFF_EXTENSIONS: &[&str] = &["tif", "tiff"];
pub const PNG_EXTENSIONS: &[&str] = &["png"];
pub const JPEG_EXTENSIONS: &[&str] = &["jpg", "jpeg"];
pub const RAW_EXTENSIONS: &[&str] = &[
    "ari", "arw", "cr2", "cr3", "crm", "crw", "dcr", "dcs", "dng", "erf", "iiq", "kdc", "mef",
    "mos", "mrw", "nef", "nrw", "orf", "ori", "pef", "raf", "raw", "rw2", "rwl", "srw", "3fr",
    "fff", "x3f", "qtk",
];

/// List TIFF and RAW frames using the same shallow `frames/` convention.
pub fn all_frame_paths(dir: &Path) -> Result<Vec<PathBuf>, TiffError> {
    let mut extensions = Vec::with_capacity(TIFF_EXTENSIONS.len() + RAW_EXTENSIONS.len());
    extensions.extend_from_slice(TIFF_EXTENSIONS);
    extensions.extend_from_slice(RAW_EXTENSIONS);
    scan_frames(dir, &extensions, |path| {
        !has_extension(path, TIFF_EXTENSIONS) || is_frame_path(path)
    })
}

/// [`frame_paths_for_extensions`] narrowed to the paths `keep` accepts;
/// `NoFrames` when nothing survives, naming `dir/frames` when it exists.
pub(crate) fn scan_frames(
    dir: &Path,
    extensions: &[&str],
    keep: impl Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, TiffError> {
    let paths: Vec<_> = frame_paths_for_extensions(dir, extensions)?
        .into_iter()
        .filter(|path| keep(path))
        .collect();
    if paths.is_empty() {
        let frames = dir.join("frames");
        return Err(TiffError::NoFrames {
            path: if frames.is_dir() {
                frames
            } else {
                dir.to_path_buf()
            },
        });
    }
    Ok(paths)
}

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

    #[test]
    fn all_frames_skips_prior_tiff_outputs_in_frames_directory() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let frames = dir.path().join("frames");
        std::fs::create_dir(&frames).expect("frames directory");
        for name in ["z.DNG", "a.TIFF", "composite.tif", "zmap.tif"] {
            std::fs::write(frames.join(name), b"frame").expect("frame file");
        }

        let paths = all_frame_paths(dir.path()).expect("recognized source frames");
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

    #[test]
    fn all_frames_refuses_a_directory_with_only_prior_tiff_outputs() {
        let dir = tempfile::tempdir().expect("temporary directory");
        for name in ["composite.tif", "zmap.tif"] {
            std::fs::write(dir.path().join(name), b"output").expect("output file");
        }

        assert!(matches!(
            all_frame_paths(dir.path()),
            Err(TiffError::NoFrames { path }) if path == dir.path()
        ));
    }
}
