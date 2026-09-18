// SPDX-License-Identifier: GPL-3.0-or-later

//! A folder becomes projects: classifying what a picker, a drop or `--open`
//! handed in, and turning it into one or more per-tab projects.
//!
//! Plain Rust, no `use slint::` -- the one module in the crate that names
//! `focusforge_core::io::grouping` / `focusforge_core::project`, the way
//! `crate::gpu_canvas` is the one naming `wgpu` and `crate::bridge` the one
//! naming `winit`. The two entrances (the picker, the drop route) and
//! `--open` all resolve through `classify` and `open` so they cannot
//! diverge (item Objective).

use std::path::{Path, PathBuf};

use focusforge_core::io::{Group, TiffError, TiffHeader, is_frame_path, one_group, propose};
use focusforge_core::params::StackContext;
use focusforge_core::project::{PROJECT_EXTENSION, Project, ProjectError};
// Named here, and re-exported, so `controller.rs` can carry a `Proposal` in
// its own pending-question state without naming `focusforge_core` itself
// (`tests/shell.rs`'s crate-wide scan).
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
/// `zip::ZipArchive<File>` inline.
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

/// The three shapes opening a path set can take. `Question` carries the
/// [`Proposal`] behind it alongside the question's own rendering summary,
/// so the controller can answer it later without re-walking the directory
/// (step 11's `pending: Option<(GroupingQuestion, Proposal)>`).
pub enum OpenOutcome {
    Opened(Opened),
    Question(GroupingQuestion, Proposal),
    Refused(EntryError),
}

/// Every way the entry path can refuse, each carrying its own
/// already-rendered message: `perform` (`crate::bridge`) must not format
/// user-facing text.
#[derive(Debug, thiserror::Error)]
pub enum EntryError {
    /// Names the format read and offers the way out -- the whole message
    /// this variant's `Display` produces is what the refusal dialog and the
    /// headless controller test both read (AC: "says so, names the format
    /// it reads, and offers `Pick another folder…`").
    #[error(
        "FocusForge reads 16-bit TIFF (.tif, .tiff). This folder holds none. \
         Pick another folder…"
    )]
    NoReadableFrames { path: PathBuf },
    /// Any other failure reading the folder's frames.
    #[error("{}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: TiffError,
    },
    /// A path with the project extension that will not open as a project.
    #[error("{}: not a FocusForge project ({source})", path.display())]
    NotAProject {
        path: PathBuf,
        #[source]
        source: ProjectError,
    },
    /// Nothing this entry path recognises: not a folder, not a `.ffp`, not
    /// TIFF frames.
    #[error("{}: not a folder of frames or a FocusForge project", path.display())]
    Unreadable { path: PathBuf },
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

fn has_project_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXTENSION))
}

/// Resolves one [`OpenTarget`] into an [`OpenOutcome`].
///
/// `Folder` proposes a grouping over `dir`: one group opens straight
/// through, more than one raises the one question. `Frames` builds its
/// project from exactly the files given, no sibling scan. `Project` reopens
/// a saved `.ffp`.
pub fn open(target: OpenTarget) -> OpenOutcome {
    match target {
        OpenTarget::Folder(dir) => match propose(&dir) {
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
            Err(source) => OpenOutcome::Refused(EntryError::Read { path: dir, source }),
        },
        OpenTarget::Frames { dir, frames } => OpenOutcome::Opened(Opened {
            projects: vec![GuiProject::Unsaved(one_group(&dir, frames))],
            skipped: Vec::new(),
        }),
        OpenTarget::Project(path) => match Project::open(&path) {
            Ok(project) => OpenOutcome::Opened(Opened {
                projects: vec![GuiProject::Opened(Box::new(project))],
                skipped: Vec::new(),
            }),
            Err(source) => OpenOutcome::Refused(EntryError::NotAProject { path, source }),
        },
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

/// The first of an opened project's recorded sources whose resolved path is
/// not a file, if any -- what a re-fuse refuses on, deliberately narrower
/// than [`Project::unresolved_sources`], which also reports every source
/// recorded `AsGiven` even when it is perfectly present on disk.
pub fn missing_source(project: &Project) -> Option<PathBuf> {
    project
        .manifest()
        .sources
        .iter()
        .map(|source| project.source_path(source))
        .find(|path| !path.is_file())
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
        GuiProject::Opened(project) => project
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.path().display().to_string()),
    }
}
