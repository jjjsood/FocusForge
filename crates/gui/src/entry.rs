// SPDX-License-Identifier: GPL-3.0-or-later

//! A folder becomes projects: classifying what a picker, a drop or `--open`
//! handed in, and turning it into one or more per-tab projects.
//!
//! Plain Rust, no `use slint::` -- the one module in the crate that names
//! `focusforge_core::io::grouping` / `focusforge_core::project`, the way
//! `crate::gpu_canvas` is the one naming `wgpu` and `crate::bridge` the one
//! naming `winit`. The two entrances (the picker, the drop route) and
//! `--open` all resolve through `classify` and `open` so they cannot
//! diverge.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use focusforge_core::io::{
    Group, GroupSignal, TiffError, TiffHeader, frame_paths, is_frame_path, one_group,
    propose_with_progress,
};
use focusforge_core::params::StackContext;
use focusforge_core::project::{
    Opened as CoreOpened, PROJECT_EXTENSION, PendingProject, Project, ProjectError, SourceRef,
    check_sources, source_hash,
};
// Re-exported so `controller.rs` can carry a `Proposal` in its own
// pending-question state without naming `focusforge_core` itself.
pub use focusforge_core::io::Proposal;

/// What a set of paths (a picker result, a drop gesture, `--open`'s single
/// argument) resolves to, before any grouping question is asked.
#[derive(Debug, Clone)]
pub enum OpenTarget {
    /// A directory to propose a grouping over.
    Folder(PathBuf),
    /// Exactly the frames given, from one common directory -- a multi-file
    /// drop. No sibling scan: the dropped files *are* the project.
    Frames { dir: PathBuf, frames: Vec<PathBuf> },
    /// A saved `*.ffp` to reopen.
    Project(PathBuf),
}

/// The one grouping question's two answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAs {
    OneProject,
    NProjects,
}

/// What the one grouping question needs to render itself. Carries no
/// grouping vocabulary of its own -- "stacks" and "frames", never "groups":
/// no string in the shell reads `group`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupingQuestion {
    pub root: PathBuf,
    pub frame_count: usize,
    pub project_count: usize,
}

/// One tab's project state: not yet fused, or reopened from a saved `.ffp`.
/// `Opened` is boxed so a `Vec<ProjectTab>` growth never copies a
/// `Project` (its file handle, record index and pending write) inline.
pub enum GuiProject {
    Unsaved(Group),
    Opened(Box<Project>),
}

/// What a successful open produced: the projects it opened (one or more
/// tabs) plus any subfolders the walk skipped -- reported, not silently
/// dropped.
pub struct Opened {
    pub projects: Vec<GuiProject>,
    pub skipped: Vec<PathBuf>,
}

/// A saved project blocked by a fingerprint mismatch: the [`PendingProject`]
/// core produced, the `.ffp` path, its file name and its recorded sources.
/// `pending` is `None` only on the cheap echo handed back once the state
/// has moved the original in; [`Self::proceed`] is called only on that
/// original.
pub struct PendingOpen {
    pending: Option<PendingProject>,
    pub path: PathBuf,
    pub file_name: String,
    /// The manifest's own recorded sources -- `PendingProject` exposes none
    /// of its manifest, only its mismatch, so `Locate frames…` on this
    /// dialog verifies against this copy instead ([`verify_folder`]).
    pub sources: Vec<SourceRef>,
}

impl PendingOpen {
    fn new(path: PathBuf, pending: PendingProject) -> Self {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // A second, cheap reopen just for the manifest's own sources --
        // `PendingProject` exposes no manifest access, only its mismatch.
        let sources = Project::open(&path)
            .map(|p| p.manifest().sources.clone())
            .unwrap_or_default();
        PendingOpen {
            pending: Some(pending),
            path,
            file_name,
            sources,
        }
    }

    /// How many frames the project was recorded over.
    pub fn frame_count(&self) -> usize {
        self.sources.len()
    }

    /// A cheap echo of `self`, carrying no live [`PendingProject`] -- what
    /// the controller hands back once it has moved the real one into its
    /// own state.
    pub(crate) fn echo(&self) -> Self {
        PendingOpen {
            pending: None,
            path: self.path.clone(),
            file_name: self.file_name.clone(),
            sources: self.sources.clone(),
        }
    }

    /// Accepts the mismatch and opens the project unchanged.
    pub fn proceed(self) -> GuiProject {
        GuiProject::Opened(Box::new(
            self.pending
                .expect("proceed is only called on the state-held pending mismatch")
                .proceed(),
        ))
    }
}

/// The four shapes opening a path set can take. `Question` carries the
/// [`Proposal`] behind it alongside the question's own rendering summary,
/// so the controller can answer it later without re-walking the directory
/// (`pending: Option<(GroupingQuestion, Proposal)>`).
pub enum OpenOutcome {
    Opened(Opened),
    Question(GroupingQuestion, Proposal),
    /// A saved project whose source content no longer matches the
    /// fingerprint it was recorded with -- blocked until the mismatch is
    /// resolved. Boxed: a live [`Project`] dwarfs every other variant.
    Mismatch(Box<PendingOpen>),
    Refused(EntryError),
}

/// Every way the entry path can refuse, each carrying its own
/// already-rendered message: `perform` (`crate::bridge`) must not format
/// user-facing text.
#[derive(Debug, thiserror::Error)]
pub enum EntryError {
    /// Names the formats read. This `Display` is the refusal dialog's whole
    /// body; the dialog's button offers the way out.
    #[error(
        "FocusForge reads 16-bit TIFF (.tif, .tiff) and camera RAW files. This folder holds none."
    )]
    NoReadableFrames { path: PathBuf },
    /// Any other failure reading the folder's frames.
    #[error("{}: {source}", path.display())]
    Read {
        path: PathBuf,
        // Boxed: `TiffError` is larger on Windows, where an `OsString` is 32 bytes.
        #[source]
        source: Box<TiffError>,
    },
    /// A path with the project extension that will not open as a project,
    /// its message worded for the actual cause rather than one fixed
    /// sentence for every failure.
    #[error("{}", refusal_message(source))]
    ProjectRefused {
        path: PathBuf,
        #[source]
        source: ProjectError,
    },
    /// Nothing this entry path recognises: not a folder, not a `.ffp`, not
    /// TIFF frames.
    #[error("{}: not a folder of frames or a FocusForge project", path.display())]
    Unreadable { path: PathBuf },
}

/// [`EntryError::ProjectRefused`]'s own cause text: one sentence per group
/// of [`ProjectError`] causes, none naming a zip entry, a blob, a delta or a
/// snapshot -- the user is told what is wrong, not how the container is
/// laid out. `ProjectError` is `#[non_exhaustive]`, so every cause this
/// match does not name falls back to the damaged-history sentence, the
/// safest thing to say about an unrecognised failure.
fn refusal_message(source: &ProjectError) -> String {
    match source {
        ProjectError::NotAProject { .. } => "This file is not a FocusForge project.".to_string(),
        ProjectError::Truncated { .. } => {
            "The file was cut short — it ends before the project does.".to_string()
        }
        ProjectError::Io { source, .. } => {
            format!("The file could not be read: {source}")
        }
        _ => "The project file is damaged: part of its saved history no longer reads back as it \
              was written."
            .to_string(),
    }
}

/// Why an additive import refused, each carrying an already-rendered
/// toast message -- the blocked-add toast is a plain string, never a card
/// or a dialog.
#[derive(Debug, thiserror::Error)]
pub enum ImportRefused {
    /// Nothing in the import resolves to a folder or a TIFF frame.
    #[error("No TIFF images to add")]
    NoFrames { path: PathBuf },
    /// The active tab is not in Stacking mode.
    #[error("Switch to Stacking to add images")]
    NotStacking,
    /// The active tab's own job is running.
    #[error("Adding images waits for the run to finish")]
    JobRunning,
    /// A previous import scan on this tab has not settled yet.
    #[error("Still checking the last images added")]
    StillChecking,
}

/// The one place the two entrances and `--open` agree on what a path set
/// means.
///
/// - Exactly one directory -> [`OpenTarget::Folder`].
/// - One file whose extension is [`PROJECT_EXTENSION`] -> [`OpenTarget::Project`].
/// - One or more files all passing [`is_frame_path`] -> [`OpenTarget::Frames`],
///   their common parent as `dir`. Files with no common parent are
///   [`EntryError::Unreadable`] too -- a project has one directory by
///   construction ([`Group::dir`]).
/// - Anything else -> [`EntryError::Unreadable`].
pub fn classify(paths: &[PathBuf]) -> Result<OpenTarget, EntryError> {
    if let [only] = paths {
        if only.is_dir() {
            return Ok(OpenTarget::Folder(only.clone()));
        }
        if only.is_file() && has_project_extension(only) {
            return Ok(OpenTarget::Project(only.clone()));
        }
    }

    if !paths.is_empty() && paths.iter().all(|p| p.is_file() && is_frame_path(p)) {
        let mut parents = paths.iter().filter_map(|p| p.parent());
        if let Some(first) = parents.next()
            && parents.all(|d| d == first)
        {
            return Ok(OpenTarget::Frames {
                dir: first.to_path_buf(),
                frames: paths.to_vec(),
            });
        }
    }

    Err(EntryError::Unreadable {
        path: paths.first().cloned().unwrap_or_default(),
    })
}

/// Whether `path` ends in the project extension, in any case; the disk is
/// not consulted.
pub fn has_project_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXTENSION))
}

/// Whether `paths` is exactly one saved project -- the one shape an
/// additive drop routes through the ordinary open path rather than adding
/// to the active tab, matching [`classify`]'s own single-project case.
pub fn is_lone_project(paths: &[PathBuf]) -> bool {
    matches!(paths, [only] if only.is_file() && has_project_extension(only))
}

/// Expands an import's paths into frame files: directories through
/// [`frame_paths`], files through [`is_frame_path`]; parent directories may
/// differ. Any other path refuses the whole import.
pub fn expand_import(paths: &[PathBuf]) -> Result<Vec<PathBuf>, ImportRefused> {
    let mut frames = Vec::new();
    for path in paths {
        if path.is_dir() {
            let found =
                frame_paths(path).map_err(|_| ImportRefused::NoFrames { path: path.clone() })?;
            frames.extend(found);
        } else if path.is_file() && is_frame_path(path) {
            frames.push(path.clone());
        } else {
            return Err(ImportRefused::NoFrames { path: path.clone() });
        }
    }
    let mut seen = HashSet::new();
    frames.retain(|p| seen.insert(p.clone()));
    if frames.is_empty() {
        return Err(ImportRefused::NoFrames {
            path: paths.first().cloned().unwrap_or_default(),
        });
    }
    Ok(frames)
}

/// Splits `incoming` by content hash into `(add, replace)`; `replace` pairs
/// the matching frame's index in `existing` with the incoming path. A file
/// that fails to hash is always added.
pub fn diff_import(
    existing: &[PathBuf],
    incoming: Vec<PathBuf>,
) -> (Vec<PathBuf>, Vec<(usize, PathBuf)>) {
    let mut existing_by_hash: HashMap<String, usize> = HashMap::new();
    for (i, frame) in existing.iter().enumerate() {
        if let Ok((_, hash)) = source_hash(frame, None) {
            existing_by_hash.entry(hash).or_insert(i);
        }
    }
    let mut add = Vec::new();
    let mut replace = Vec::new();
    let mut seen = HashSet::new();
    for path in incoming {
        match source_hash(&path, None) {
            Ok((_, hash)) if seen.insert(hash.clone()) => match existing_by_hash.get(&hash) {
                Some(&position) => replace.push((position, path)),
                None => add.push(path),
            },
            Ok(_) => {} // a repeat of an already-seen incoming file's content
            Err(_) => add.push(path),
        }
    }
    (add, replace)
}

/// Resolves one [`OpenTarget`] into an [`OpenOutcome`], reporting
/// `progress(frames_read, frame_total)` as it goes -- the caller's worker
/// thread is what turns this into a loading indicator's ticks.
///
/// `Folder` proposes a grouping over `dir`, reading one frame header per
/// report; one group opens straight through, more than one raises the one
/// question. `Frames` and `Project` read no frame headers at all, so each
/// reports exactly once, already done. `Frames` builds its project from
/// exactly the files given, no sibling scan. `Project` reopens a saved
/// `.ffp`.
pub fn open(target: OpenTarget, mut progress: impl FnMut(usize, usize)) -> OpenOutcome {
    match target {
        OpenTarget::Folder(dir) => match propose_with_progress(&dir, &mut progress) {
            Ok(proposal) => {
                if proposal.groups.len() == 1 {
                    OpenOutcome::Opened(Opened {
                        projects: proposal
                            .groups
                            .into_iter()
                            .map(GuiProject::Unsaved)
                            .collect(),
                        skipped: proposal.skipped,
                    })
                } else {
                    let question = GroupingQuestion {
                        frame_count: proposal.groups.iter().map(|g| g.frames.len()).sum(),
                        project_count: proposal.groups.len(),
                        root: proposal.root.clone(),
                    };
                    OpenOutcome::Question(question, proposal)
                }
            }
            Err(TiffError::NoFrames { path }) => {
                OpenOutcome::Refused(EntryError::NoReadableFrames { path })
            }
            Err(source) => OpenOutcome::Refused(EntryError::Read {
                path: dir,
                source: Box::new(source),
            }),
        },
        OpenTarget::Frames { dir, frames } => {
            progress(frames.len(), frames.len());
            OpenOutcome::Opened(Opened {
                projects: vec![GuiProject::Unsaved(one_group(&dir, frames))],
                skipped: Vec::new(),
            })
        }
        OpenTarget::Project(path) => {
            progress(1, 1);
            match Project::open_checked(&path) {
                Ok(CoreOpened::Ready(project)) => OpenOutcome::Opened(Opened {
                    projects: vec![GuiProject::Opened(Box::new(project))],
                    skipped: Vec::new(),
                }),
                Ok(CoreOpened::NeedsChoice(pending)) => {
                    OpenOutcome::Mismatch(Box::new(PendingOpen::new(path, pending)))
                }
                Err(source) => OpenOutcome::Refused(EntryError::ProjectRefused { path, source }),
            }
        }
    }
}

/// Answers the one grouping question against the [`Proposal`] [`open`]
/// raised it with. `OneProject` folds every group's frames, in proposal
/// order, into one project; `NProjects` opens each group as its own tab.
/// Both clear the question's own vocabulary: the caller never sees a
/// [`Group`] again after this call, only [`GuiProject`]s.
pub fn answer_question(proposal: &Proposal, answer: OpenAs) -> Opened {
    let projects = match answer {
        OpenAs::OneProject => {
            let frames: Vec<PathBuf> = proposal
                .groups
                .iter()
                .flat_map(|g| g.frames.iter().cloned())
                .collect();
            vec![GuiProject::Unsaved(one_group(&proposal.root, frames))]
        }
        OpenAs::NProjects => proposal
            .groups
            .iter()
            .cloned()
            .map(GuiProject::Unsaved)
            .collect(),
    };
    Opened {
        projects,
        skipped: proposal.skipped.clone(),
    }
}

/// A tab's [`StackContext`], without a composite: an [`GuiProject::Unsaved`]
/// tab reads its first frame's header only ([`TiffHeader::open`], the same
/// read `grouping::read_capture_time` already does) plus the group's own
/// frame count; a reopened [`GuiProject::Opened`] reads the sealed
/// manifest's composite plane instead. Cached once by `ProjectTab::new`,
/// never re-read on every settings-panel resolve. `None` for an unreadable
/// or frame-less group, which `propose`/`one_group` never actually produce
/// but a future caller must not panic on.
pub fn tab_stack_context(project: &GuiProject) -> Option<StackContext> {
    match project {
        GuiProject::Unsaved(group) => {
            let header = TiffHeader::open(group.frames.first()?).ok()?;
            Some(StackContext::new(
                group.frames.len(),
                header.width,
                header.height,
                header.samples_per_pixel,
            ))
        }
        GuiProject::Opened(project) => {
            let composite = &project.manifest().composite;
            Some(StackContext::new(
                composite.frame_count as usize,
                composite.width,
                composite.height,
                composite.samples_per_pixel,
            ))
        }
    }
}

/// A tab's own frame list, in frame order: the group's own `frames` for an
/// [`GuiProject::Unsaved`] tab; for a reopened [`GuiProject::Opened`] one,
/// its manifest's recorded sources mapped through [`Project::source_path`].
/// What a re-fuse assembles its request from.
pub fn project_frames(project: &GuiProject) -> Vec<PathBuf> {
    match project {
        GuiProject::Unsaved(group) => group.frames.clone(),
        GuiProject::Opened(project) => project
            .manifest()
            .sources
            .iter()
            .map(|source| project.source_path(source))
            .collect(),
    }
}

/// A tab's own source-frame size, read from an actual frame's header --
/// always the frames' own extent, never the sealed composite's (possibly
/// cropped) one, for a fresh group and for a reopened project alike.
pub fn tab_image_size(project: &GuiProject) -> Option<(u32, u32)> {
    let frames = project_frames(project);
    let header = TiffHeader::open(frames.first()?).ok()?;
    Some((header.width, header.height))
}

/// A tab's frame count: the group's own frame count pre-fuse, the sealed
/// composite's recorded one once opened.
pub fn tab_frame_count(project: &GuiProject) -> u32 {
    match project {
        GuiProject::Unsaved(group) => group.frames.len() as u32,
        GuiProject::Opened(project) => project.manifest().composite.frame_count,
    }
}

/// A tab's title: the group's own derived name pre-fuse, the project
/// file's own stem once opened -- a saved project has no [`Group`] to name
/// it from any more.
pub fn tab_title(project: &GuiProject) -> String {
    match project {
        GuiProject::Unsaved(group) => group.name.clone(),
        GuiProject::Opened(project) => project_title(project.path()),
    }
}

/// The name a saved project at `path` shows: the file's stem.
pub fn project_title(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A fresh `Unsaved` project over `frames`, named `name` inside `dir` and
/// carrying no capture-time signal of its own -- what "New project
/// instead" opens: the same frames a frame-set-change warning was about to
/// touch, with no checkpoint of their own.
pub fn duplicate_as_unsaved(dir: PathBuf, name: String, frames: Vec<PathBuf>) -> GuiProject {
    GuiProject::Unsaved(Group {
        name,
        dir,
        frames,
        signal: GroupSignal::Directory,
        first_time: None,
        last_time: None,
    })
}

/// `folder`'s own candidate path for `source`, by recorded file name -- the
/// one place a located folder's file names are joined against.
fn candidate_path(source: &SourceRef, folder: &Path) -> PathBuf {
    let name = Path::new(&source.path)
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&source.path));
    folder.join(name)
}

/// Why a located folder was refused: how many of `total` sources it could
/// not find at all, and how many it found but whose content no longer
/// hashes to what was recorded.
pub struct LocateRefused {
    pub missing: usize,
    pub changed: usize,
    pub total: usize,
}

impl LocateRefused {
    /// The refused-locate sentence, preferring the changed count when both
    /// are nonzero -- a same-named but different file is a stronger signal
    /// than one simply missing.
    pub fn message(&self) -> String {
        if self.changed > 0 {
            format!(
                "{} of {} frames in this folder differ from the ones this project was made over",
                self.changed, self.total
            )
        } else {
            format!(
                "{} of {} cannot be found in this folder",
                self.missing, self.total
            )
        }
    }
}

/// What a `Locate frames…` resolution verifies against: the pending
/// mismatch's own recorded sources, or an already-open tab's -- named by
/// its own stable id, never a strip index that a close or reorder can
/// shift out from under a verification still running on its own thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocateTarget {
    Mismatch,
    Tab(u64),
}

/// Reopens `path` plain and relocates its sources to `folder` -- what a
/// verified `Locate frames…` resolution applies to a pending mismatch: a
/// folder [`verify_folder`] accepted matches the project's own recorded
/// content, only at a different location, so nothing is recorded as a
/// mismatch.
pub fn reopen_relocated(path: &Path, folder: PathBuf) -> Option<GuiProject> {
    let mut project = Project::open(path).ok()?;
    project.relocate_sources(folder);
    Some(GuiProject::Opened(Box::new(project)))
}

/// Verifies `folder` against `sources` by content hash
/// ([`check_sources`]) -- never by file name alone, so a folder holding
/// same-named but different frames is refused rather than silently
/// accepted. `Ok` only once every source resolves and hashes unchanged,
/// giving back each source's own path inside `folder`, in source order.
pub fn verify_folder(sources: &[SourceRef], folder: &Path) -> Result<Vec<PathBuf>, LocateRefused> {
    let total = sources.len();
    let check =
        check_sources(sources, |s| candidate_path(s, folder)).map_err(|_| LocateRefused {
            missing: total,
            changed: 0,
            total,
        })?;
    if check.unresolved.is_empty() && check.changed.is_empty() {
        Ok(sources.iter().map(|s| candidate_path(s, folder)).collect())
    } else {
        Err(LocateRefused {
            missing: check.unresolved.len(),
            changed: check.changed.len(),
            total,
        })
    }
}

/// How many of `project`'s sources this open cannot resolve to a file on
/// disk, out of how many it has in total -- the honest, counted fact behind
/// the missing-sources card and dialog. Counts only what is actually
/// unreachable, unlike [`Project::unresolved_sources`], which also counts
/// every source recorded [`focusforge_core::project::SourceResolution::AsGiven`]
/// whether or not it is still reachable from here.
pub fn missing_sources(project: &Project) -> (usize, usize) {
    let sources = &project.manifest().sources;
    let missing = sources
        .iter()
        .filter(|s| !project.source_path(s).is_file())
        .count();
    (missing, sources.len())
}
