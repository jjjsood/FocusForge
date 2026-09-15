// SPDX-License-Identifier: GPL-3.0-or-later

//! `--groups` / `--write-groups`: the grouping file, the printed proposal
//! and the run-level manifest.
//!
//! The rule itself lives in `core` ([`focusforge_core::io::propose`]) --
//! the GUI consumes it without linking this crate. What lives here is
//! the half `crates/cli` owns: flags, a file format, terminal output and
//! output paths.
//!
//! # The grouping file
//!
//! TOML, a scalar `format` key followed by one `[[group]]` block per stack,
//! with `frames` holding paths **relative to the directory the run was
//! pointed at**:
//!
//! ```toml
//! format = 1
//!
//! [[group]]
//! name = "IMG_9801-9862"
//! frames = [
//!     "IMG_9801.tif",
//!     "IMG_9802.tif",
//! ]
//!
//! [[group]]
//! name = "IMG_9910-9931"
//! frames = ["IMG_9910.tif"]
//! ```
//!
//! `--write-groups <path>` writes exactly what `--groups <path>` reads, so
//! writing and feeding the unedited file back is a no-op.
//!
//! **Field order is load-bearing**, one notch worse than the preset's
//! (`crate::preset`): a scalar emitted *after* an array of tables lands
//! inside the **last** `[[group]]` block, and `toml::ser::Error` is opaque
//! in 1.1.6, so there is no variant to match on. `format` is declared first
//! and `groups_file_round_trips_with_the_scalar_outside_the_array` (below)
//! is the guard.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Args;
use focusforge_core::io::{Group, GroupSignal, Proposal};
use serde::{Deserialize, Serialize};

/// The grouping file's schema version, additive-only from here forward.
pub const GROUPS_FILE_FORMAT: u32 = 1;

/// The run-level manifest's schema version.
pub const MANIFEST_FORMAT: u32 = 1;

/// The two grouping flags, flattened into `stack` -- the shape
/// [`crate::set::SetArgs`] established, where the type is the reusable
/// thing rather than a position on the command line.
#[derive(Args, Debug, Default)]
pub struct GroupArgs {
    /// Group the frames as this TOML file says instead of proposing a
    /// grouping: `format = 1` plus one `[[group]]` block per stack, each
    /// with a `name` and a `frames` array of paths relative to `<dir>`.
    /// Every frame found under `<dir>` must appear in exactly one group.
    #[arg(long, value_name = "PATH")]
    pub groups: Option<PathBuf>,
    /// Write the proposed grouping to this path, in exactly the format
    /// `--groups` reads, then carry on with the run. Edit it and feed it
    /// back with `--groups` to override the proposal.
    #[arg(long = "write-groups", value_name = "PATH")]
    pub write_groups: Option<PathBuf>,
}

/// One grouping file.
///
/// `format` is declared **before** `group` -- see this module's docs for why
/// that is a correctness property and not a style choice.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct GroupsFile {
    pub format: u32,
    #[serde(default)]
    pub group: Vec<GroupEntry>,
}

/// One `[[group]]` block.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct GroupEntry {
    pub name: String,
    pub frames: Vec<String>,
}

/// The run-level manifest: every **attempted** group's identity and
/// outcome, in one file, so a group that failed is a listed failure rather
/// than a missing output.
///
/// Its per-group shape is [`crate::status::StatusRecord`]'s plus the name
/// and the frame count, so the two artifacts cannot drift into two
/// spellings of one fact. Written for **every** run, single-group included:
/// `review` reads this file for the commonest invocation too, so "a failed
/// stack is a visible failure" cannot hold only for grouped runs.
#[derive(Serialize, Deserialize, Debug)]
pub struct RunManifest {
    pub format: u32,
    /// The input directory as the user wrote it.
    pub root: String,
    pub groups: Vec<ManifestEntry>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ManifestEntry {
    pub name: String,
    pub frame_count: usize,
    /// `completed`, `failed`, `cancelled` or `dry-run` -- the same
    /// vocabulary [`crate::status::StatusRecord::outcome`] uses.
    pub outcome: String,
    pub outputs: Vec<String>,
    pub wall_seconds: f64,
    pub error: Option<String>,
}

/// The proposal as a grouping file: frames relative to `proposal.root`
/// where they sit under it, absolute (well, as-walked) otherwise.
pub fn to_file(proposal: &Proposal) -> GroupsFile {
    GroupsFile {
        format: GROUPS_FILE_FORMAT,
        group: proposal
            .groups
            .iter()
            .map(|group| GroupEntry {
                name: group.name.clone(),
                frames: group
                    .frames
                    .iter()
                    .map(|p| relative_to(&proposal.root, p))
                    .collect(),
            })
            .collect(),
    }
}

/// Serializes the proposal to `path` in the format [`load`] reads.
pub fn write(proposal: &Proposal, path: &Path) -> Result<(), String> {
    let text = toml::to_string_pretty(&to_file(proposal))
        .map_err(|err| format!("{}: serializing the grouping: {err}", path.display()))?;
    std::fs::write(path, text).map_err(|err| format!("{}: {err}", path.display()))
}

/// Replaces `walked` with the grouping `path` names.
///
/// Three named failure modes, all checked before any frame is opened and
/// each naming the offending frame: a listed frame that the walk did not
/// find, a frame listed in two groups, and a frame the walk found that no
/// group lists. Comparison is by `std::path::absolute` of `root.join(entry)`
/// -- lexical normalization, the idiom `crate::stack::colliding_frame`
/// already uses, which needs no file to exist.
pub fn load(path: &Path, root: &Path, walked: &Proposal) -> Result<Proposal, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let file: GroupsFile =
        toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    if file.format != GROUPS_FILE_FORMAT {
        return Err(format!(
            "{}: unsupported grouping file format {} (this build reads {GROUPS_FILE_FORMAT})",
            path.display(),
            file.format
        ));
    }
    if file.group.is_empty() {
        return Err(format!("{}: no [[group]] block at all", path.display()));
    }

    // Every frame the walk found, keyed by its normalized path.
    let mut walked_frames: BTreeMap<PathBuf, &Group> = BTreeMap::new();
    for group in &walked.groups {
        for frame in &group.frames {
            walked_frames.insert(normalize(frame), group);
        }
    }

    let mut claimed: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for entry in &file.group {
        if entry.name.trim().is_empty() {
            return Err(format!("{}: a [[group]] has an empty name", path.display()));
        }
        if groups.iter().any(|g| g.name == entry.name) {
            return Err(format!(
                "{}: two groups are both named {:?}",
                path.display(),
                entry.name
            ));
        }
        let mut frames = Vec::new();
        for listed in &entry.frames {
            let full = normalize(&root.join(listed));
            let Some(source) = walked_frames.get(&full) else {
                return Err(format!(
                    "{}: group {:?} lists {listed}, which is not a frame under {}",
                    path.display(),
                    entry.name,
                    root.display()
                ));
            };
            if let Some(other) = claimed.get(&full) {
                return Err(format!(
                    "{}: frame {listed} is listed twice -- in group {:?} and in group {:?}",
                    path.display(),
                    other,
                    entry.name
                ));
            }
            claimed.insert(full.clone(), entry.name.clone());
            frames.push((full, source.dir.clone()));
        }
        if frames.is_empty() {
            return Err(format!(
                "{}: group {:?} lists no frames",
                path.display(),
                entry.name
            ));
        }
        let dir = frames[0].1.clone();
        let mut paths: Vec<PathBuf> = frames.into_iter().map(|(p, _)| p).collect();
        paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        // A group the file reproduces unedited keeps the walk's own signal
        // and boundary times, so writing a proposal and feeding it straight
        // back prints the identical grouping -- the round trip is a no-op,
        // which is the whole point of `--write-groups`. A group the user
        // actually edited matches no walked group and is reported as the
        // user's own partition instead.
        let matched = walked.groups.iter().find(|g| {
            g.frames
                .iter()
                .map(|p| normalize(p))
                .eq(paths.iter().cloned())
        });
        groups.push(Group {
            name: entry.name.clone(),
            dir,
            frames: paths,
            signal: matched.map_or(GroupSignal::Directory, |g| g.signal),
            first_time: matched.and_then(|g| g.first_time.clone()),
            last_time: matched.and_then(|g| g.last_time.clone()),
        });
    }

    for (frame, _) in walked_frames.iter() {
        if !claimed.contains_key(frame) {
            return Err(format!(
                "{}: frame {} is present under {} but is in no group",
                path.display(),
                relative_to(root, frame),
                root.display()
            ));
        }
    }

    Ok(Proposal {
        root: walked.root.clone(),
        groups,
        gap: walked.gap,
        time_signal_available: walked.time_signal_available,
    })
}

/// The printed proposal: one group per line, the threshold and every
/// boundary's own gap named, so a boundary a user cannot account for does
/// not exist. `from` is the `--groups` path when a file replaced the
/// proposal.
///
/// A `String`, not prints: unit-testable without capturing stderr, the shape
/// `crate::stack`'s own renderers use.
pub fn render_proposal(proposal: &Proposal, from: Option<&Path>) -> String {
    let reason = match from {
        Some(path) => format!("from {}", path.display()),
        None if proposal.time_signal_available => format!(
            "capture-time gap > {}s, or a directory boundary",
            proposal.gap.as_secs()
        ),
        None => "directory structure (no frame carries a readable capture time)".to_string(),
    };
    let mut lines = vec![format!(
        "grouping {}: {} group{} ({reason})",
        proposal.root.display(),
        proposal.groups.len(),
        if proposal.groups.len() == 1 { "" } else { "s" },
    )];

    for (i, group) in proposal.groups.iter().enumerate() {
        let first = group
            .frames
            .first()
            .map(|p| file_name(p))
            .unwrap_or_else(|| "?".to_string());
        let last = group
            .frames
            .last()
            .map(|p| file_name(p))
            .unwrap_or_else(|| "?".to_string());
        let mut line = format!(
            "  {}  {} frame{}  {first} .. {last}  [{}]",
            group.name,
            group.frames.len(),
            if group.frames.len() == 1 { "" } else { "s" },
            group.signal.label(),
        );
        if let (Some(start), Some(end)) = (&group.first_time, &group.last_time) {
            line.push_str(&format!("  time {} .. {}", start.text, end.text));
        }
        lines.push(line);

        // The gap that produced this boundary, attributed to the frame it
        // falls after -- the fact that makes a boundary accountable.
        if i > 0 {
            let previous = &proposal.groups[i - 1];
            if previous.dir == group.dir
                && let (Some(before), Some(after)) = (&previous.last_time, &group.first_time)
            {
                lines.push(format!(
                    "    boundary: {}s gap after {}",
                    after.epoch_seconds - before.epoch_seconds,
                    previous
                        .frames
                        .last()
                        .map(|p| file_name(p))
                        .unwrap_or_default(),
                ));
            }
        }
    }
    lines.join("\n")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `path` written relative to `root` when it sits under it, so a written
/// grouping file is hand-editable and portable; the path as walked
/// otherwise.
fn relative_to(root: &Path, path: &Path) -> String {
    let normalized_root = normalize(root);
    let normalized = normalize(path);
    normalized
        .strip_prefix(&normalized_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Lexical normalization only: `std::path::absolute` never touches the
/// filesystem, so it works on a path whose file does not exist.
fn normalize(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusforge_core::io::CaptureTime;
    use std::time::Duration;

    fn group(name: &str, dir: &str, frames: &[&str], signal: GroupSignal) -> Group {
        Group {
            name: name.to_string(),
            dir: PathBuf::from(dir),
            frames: frames.iter().map(|f| PathBuf::from(dir).join(f)).collect(),
            signal,
            first_time: None,
            last_time: None,
        }
    }

    fn timed(mut group: Group, first: &str, last: &str, first_s: i64, last_s: i64) -> Group {
        group.first_time = Some(CaptureTime {
            epoch_seconds: first_s,
            text: first.to_string(),
        });
        group.last_time = Some(CaptureTime {
            epoch_seconds: last_s,
            text: last.to_string(),
        });
        group
    }

    fn sample_proposal() -> Proposal {
        Proposal {
            root: PathBuf::from("card-dump"),
            groups: vec![
                timed(
                    group(
                        "IMG_9801-9862",
                        "card-dump",
                        &["IMG_9801.tif", "IMG_9862.tif"],
                        GroupSignal::CaptureTime,
                    ),
                    "2026:08:29 18:42:41",
                    "2026:08:29 18:44:03",
                    1000,
                    1082,
                ),
                timed(
                    group(
                        "IMG_9910-9931",
                        "card-dump",
                        &["IMG_9910.tif", "IMG_9931.tif"],
                        GroupSignal::CaptureTime,
                    ),
                    "2026:08:29 19:02:55",
                    "2026:08:29 19:03:40",
                    2176,
                    2221,
                ),
            ],
            gap: Duration::from_secs(60),
            time_signal_available: true,
        }
    }

    #[test]
    fn render_proposal_names_every_group_its_range_and_the_threshold() {
        let text = render_proposal(&sample_proposal(), None);
        for needle in [
            "grouping card-dump: 2 groups",
            "capture-time gap > 60s",
            "IMG_9801-9862",
            "2 frames",
            "IMG_9801.tif .. IMG_9862.tif",
            "time 2026:08:29 18:42:41 .. 2026:08:29 18:44:03",
            "IMG_9910-9931",
            "boundary: 1094s gap after IMG_9862.tif",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn render_proposal_names_the_file_when_a_grouping_file_replaced_it() {
        let text = render_proposal(&sample_proposal(), Some(Path::new("groups.toml")));
        assert!(text.contains("from groups.toml"), "{text}");
        assert!(!text.contains("capture-time gap >"), "{text}");
    }

    #[test]
    fn render_proposal_says_when_no_frame_carried_a_time() {
        let mut proposal = sample_proposal();
        proposal.time_signal_available = false;
        proposal.groups.truncate(1);
        proposal.groups[0].first_time = None;
        proposal.groups[0].last_time = None;
        proposal.groups[0].signal = GroupSignal::Directory;
        let text = render_proposal(&proposal, None);
        assert!(
            text.contains("no frame carries a readable capture time"),
            "the fallback must be stated, not inferred:\n{text}"
        );
        assert!(
            text.contains("1 group ("),
            "singular, not `1 groups`: {text}"
        );
    }

    /// The array-of-tables twin of
    /// `preset::tests::preset_round_trips_with_scalars_outside_both_tables`,
    /// one notch worse: a scalar emitted after `[[group]]` lands inside the
    /// **last** group, where it would deserialize as an unknown field.
    #[test]
    fn groups_file_round_trips_with_the_scalar_outside_the_array() {
        let file = to_file(&sample_proposal());
        let text = toml::to_string_pretty(&file).expect("a grouping file must serialize");
        let format_line = text.find("format =").expect("format must be written");
        let first_table = text.find('[').expect("a [[group]] header must be written");
        assert!(
            format_line < first_table,
            "`format` must precede every [[group]] header, else it lands inside one:\n{text}"
        );
        let back: GroupsFile = toml::from_str(&text).expect("a written grouping must re-read");
        assert_eq!(back, file);
        assert_eq!(back.group[0].frames, vec!["IMG_9801.tif", "IMG_9862.tif"]);
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let err = toml::from_str::<GroupsFile>(
            "format = 1\n\n[[group]]\nname = \"a\"\nframes = []\nnmae = \"typo\"\n",
        )
        .expect_err("a mistyped key must refuse");
        assert!(err.to_string().contains("nmae"), "{err}");
    }
}
