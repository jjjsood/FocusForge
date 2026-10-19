// SPDX-License-Identifier: GPL-3.0-or-later

//! The user-facing open: [`Project::open`] plus a source-content check, and
//! the three states that check can produce -- ready, a blocking choice, or a
//! ready project carrying a warning. [`check_sources`] is the pure content
//! check underneath it, reusable against any located folder without a
//! project reopen.

use std::path::{Path, PathBuf};

use super::Project;
use super::blob;
use super::error::ProjectError;
use super::manifest::SourceRef;

/// One source's content, checked against its recorded hash at a located
/// path: which indices could not be found at all, and which were found but
/// hash differently.
pub struct SourceCheck {
    /// Indices into the source list whose located path is not a file.
    pub unresolved: Vec<usize>,
    /// Indices, with the path they were located at, whose content no longer
    /// hashes to the recorded value.
    pub changed: Vec<(usize, PathBuf)>,
    /// The full source list with every hash replaced by what was actually
    /// read, `Some` only when every source resolved -- what
    /// [`PendingProject::recomputed_fingerprint`] is computed over.
    rehashed: Option<Vec<SourceRef>>,
}

/// Checks each of `sources` at the path `locate` returns for it. A file whose
/// stamp matches the record is unchanged without being read; any other is
/// hashed ([`blob::source_hash`]) and compared with the recorded hash.
///
/// An in-place edit that keeps size, modification time and file id is not
/// detected.
pub fn check_sources(
    sources: &[SourceRef],
    locate: impl Fn(&SourceRef) -> PathBuf,
) -> Result<SourceCheck, ProjectError> {
    let mut unresolved = Vec::new();
    let mut changed = Vec::new();
    let mut rehashed = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let path = locate(source);
        if !path.is_file() {
            unresolved.push(index);
            rehashed.push(source.clone());
            continue;
        }
        let (_, hash) = blob::source_hash(&path, Some(source))?;
        if hash != source.hash {
            changed.push((index, path.clone()));
        }
        let mut updated = source.clone();
        updated.hash = hash;
        rehashed.push(updated);
    }
    let rehashed = unresolved.is_empty().then_some(rehashed);
    Ok(SourceCheck {
        unresolved,
        changed,
        rehashed,
    })
}

/// A stack-fingerprint mismatch [`PendingProject::proceed`] accepted, and
/// what it accepted: the frames that changed, the fingerprint the project
/// was written with, and the one its sources hash to now -- `None` when a
/// source was also unresolved, so no full recomputation is possible.
#[derive(Debug, Clone)]
pub struct FingerprintMismatch {
    pub changed: Vec<(usize, PathBuf)>,
    pub recorded_fingerprint: String,
    pub recomputed_fingerprint: Option<String>,
}

/// [`Project::validation`]'s result: the warnings a project carries, as data
/// rather than a printed message.
pub struct Validation {
    /// How many of the project's sources this open cannot resolve.
    pub unresolved_sources: usize,
    pub accepted_mismatch: Option<FingerprintMismatch>,
}

/// [`Project::open_checked`]'s result: either the project is ready to use,
/// or its sources' content disagrees with what it was recorded against and
/// a caller must choose.
pub enum Opened {
    Ready(Project),
    NeedsChoice(PendingProject),
}

/// A project whose source content changed since it was recorded, blocked
/// until [`Self::proceed`] is called. Exposes only the mismatch -- no
/// history or plane access -- so nothing can read a checkpoint or a blob out
/// of an unconfirmed project.
pub struct PendingProject {
    project: Project,
    check: SourceCheck,
}

impl PendingProject {
    /// The changed frames' indices and the paths they were found at.
    pub fn changed(&self) -> &[(usize, PathBuf)] {
        &self.check.changed
    }

    /// The `stack_fingerprint` this project was written with.
    pub fn recorded_fingerprint(&self) -> &str {
        &self.project.manifest.stack_fingerprint
    }

    /// The fingerprint the sources actually hash to now, when every one of
    /// them resolved.
    pub fn recomputed_fingerprint(&self) -> Option<String> {
        self.check
            .rehashed
            .as_ref()
            .map(|sources| blob::stack_fingerprint(sources))
    }

    /// Accepts the mismatch and returns the project, its [`Project::validation`]
    /// carrying what was accepted.
    pub fn proceed(self) -> Project {
        let PendingProject { mut project, check } = self;
        let recomputed_fingerprint = check
            .rehashed
            .as_ref()
            .map(|sources| blob::stack_fingerprint(sources));
        project.accepted_mismatch = Some(FingerprintMismatch {
            changed: check.changed,
            recorded_fingerprint: project.manifest.stack_fingerprint.clone(),
            recomputed_fingerprint,
        });
        project
    }
}

impl Project {
    /// [`Project::open`] plus a source check ([`check_sources`]): `Ready`
    /// when every resolvable source is still the file it was recorded
    /// against, `NeedsChoice` when one is not. A source whose file stamp
    /// matches the record is not read; only a changed stamp costs a hash.
    /// Internal callers use plain [`Project::open`] instead.
    pub fn open_checked(path: &Path) -> Result<Opened, ProjectError> {
        let project = Project::open(path)?;
        let check = check_sources(&project.manifest.sources, |s| project.source_path(s))?;
        if check.changed.is_empty() {
            Ok(Opened::Ready(project))
        } else {
            Ok(Opened::NeedsChoice(PendingProject { project, check }))
        }
    }
}
