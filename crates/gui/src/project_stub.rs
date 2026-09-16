// SPDX-License-Identifier: GPL-3.0-or-later

//! A stub project format.
//! `--open <dir>` resolves `<dir>/golden/laplacian.tif` +
//! `<dir>/golden/laplacian-zmap.tif`, refusing a missing file by name
//! rather than a generic I/O error.

use std::path::{Path, PathBuf};

use focusforge_core::TiffError;
use focusforge_core::io::TiffStack;
use focusforge_core::zmap::{ZMapError, ZMapReader};

use crate::canvas::PaneSource;

/// Everything that can go wrong resolving `--open`'s directory into a
/// composite + Z-Map pair.
#[derive(Debug, thiserror::Error)]
pub enum ProjectStubError {
    #[error("no composite file at {}", path.display())]
    MissingComposite { path: PathBuf },
    #[error("no Z-Map file at {}", path.display())]
    MissingZMap { path: PathBuf },
    #[error("failed to open composite {}: {source}", path.display())]
    Composite {
        path: PathBuf,
        #[source]
        source: TiffError,
    },
    #[error("failed to open Z-Map {}: {source}", path.display())]
    ZMap {
        path: PathBuf,
        #[source]
        source: ZMapError,
    },
}

/// One resolved project: a composite TIFF plus its Z-Map, both under
/// `<dir>/golden/`.
#[derive(Debug, Clone)]
pub struct ProjectStub {
    composite_path: PathBuf,
    zmap_path: PathBuf,
    composite_dims: (u32, u32),
    zmap_dims: (u32, u32),
    frame_count: u32,
}

impl ProjectStub {
    /// Resolves `dir/golden/laplacian.tif` and `dir/golden/laplacian-zmap.tif`,
    /// refusing a missing file by name before ever trying to open it.
    pub fn open(dir: &Path) -> Result<Self, ProjectStubError> {
        let composite_path = dir.join("golden").join("laplacian.tif");
        let zmap_path = dir.join("golden").join("laplacian-zmap.tif");

        if !composite_path.is_file() {
            return Err(ProjectStubError::MissingComposite {
                path: composite_path,
            });
        }
        if !zmap_path.is_file() {
            return Err(ProjectStubError::MissingZMap { path: zmap_path });
        }

        let composite =
            TiffStack::open_files(std::slice::from_ref(&composite_path)).map_err(|source| {
                ProjectStubError::Composite {
                    path: composite_path.clone(),
                    source,
                }
            })?;
        let composite_dims = composite.dimensions();

        let zmap = ZMapReader::open(&zmap_path).map_err(|source| ProjectStubError::ZMap {
            path: zmap_path.clone(),
            source,
        })?;
        let zmap_dims = zmap.dimensions();
        let frame_count = zmap.frame_count();

        Ok(ProjectStub {
            composite_path,
            zmap_path,
            composite_dims,
            zmap_dims,
            frame_count,
        })
    }

    pub fn composite_path(&self) -> &Path {
        &self.composite_path
    }

    pub fn zmap_path(&self) -> &Path {
        &self.zmap_path
    }

    pub fn dimensions(&self, pane: PaneSource) -> (u32, u32) {
        match pane {
            PaneSource::Composite => self.composite_dims,
            PaneSource::ZMap => self.zmap_dims,
        }
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mantis_512_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
            .join("mantis-512")
    }

    #[test]
    fn opens_the_fixture_golden_pair() {
        let project = ProjectStub::open(&mantis_512_dir()).expect("open fixture project");
        assert_eq!(project.dimensions(PaneSource::Composite), (512, 512));
        assert_eq!(project.dimensions(PaneSource::ZMap), (512, 512));
        assert_eq!(project.frame_count(), 8);
    }

    #[test]
    fn refuses_a_missing_composite_by_name() {
        let dir = std::env::temp_dir().join("focusforge-project-stub-test-missing-composite");
        std::fs::create_dir_all(dir.join("golden")).unwrap();
        std::fs::write(dir.join("golden").join("laplacian-zmap.tif"), b"").unwrap();

        match ProjectStub::open(&dir) {
            Err(ProjectStubError::MissingComposite { path }) => {
                assert!(path.ends_with("golden/laplacian.tif"));
            }
            other => panic!("expected MissingComposite, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
