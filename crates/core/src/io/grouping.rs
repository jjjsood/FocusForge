// SPDX-License-Identifier: GPL-3.0-or-later

//! Grouping a directory of frames into stacks -- **proposed**, never
//! decided silently.
//!
//! Two signals and one precedence rule:
//!
//! 1. **Directory structure partitions first.** Every frame-bearing
//!    directory under the root is its own partition. A hand-sorted tree is
//!    therefore never re-joined, and never re-cut across directories.
//! 2. **Capture time only subdivides within one directory.** Frames whose
//!    `DateTimeOriginal` (36867) are more than [`GROUP_GAP`] apart start a
//!    new group -- the flat-card-dump case, where directory structure says
//!    nothing at all.
//!
//! The two signals cannot contradict each other by construction rather than
//! by a tie-break.
//!
//! Nothing here reads a pixel: the rule reads directory entries and TIFF
//! header tags only. Pixel-content grouping -- sharpness or similarity
//! clustering -- is excluded and is not a later extension point.
//!
//! [`propose`] is a plain library function over `std` types, with no CLI
//! type in its signature: a GUI "add project" flow consumes this same
//! rule on the same directories rather than inventing a second one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::error::TiffError;
use super::tiff::header::TiffHeader;
use super::tiff::stack::frame_paths;

/// The capture-time gap that starts a new group: frames further apart than
/// this belong to different stacks.
///
/// A **fixed** threshold, not a multiple of the observed median: measured on
/// the 61-frame reference stack, 58 of the 60 inter-frame gaps are 0 s and
/// the largest is 1 s, so a median-multiple rule degenerates to a threshold
/// of zero, while a minute leaves three orders of magnitude of headroom --
/// "seconds apart" within a stack, "minutes apart" between subjects.
///
/// It gets no flag. `--groups` is the override; a second knob for the same
/// fact is what §10's escalation framing argues against.
pub const GROUP_GAP: Duration = Duration::from_secs(60);

/// How deep the walk descends before giving up. Insurance against a symlink
/// cycle, not a real limit: no plausible frame tree is 32 levels deep.
const MAX_DEPTH: usize = 32;

/// Which signal produced a group -- reported, because a boundary the user
/// cannot account for is the silent decision §10 forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSignal {
    /// The directory it sits in, alone: no frame in it carries a usable
    /// capture time.
    Directory,
    /// A capture-time gap cut this group out of its directory's frames.
    CaptureTime,
    /// Both signals agree: the directory holds exactly one timed stack.
    Both,
    /// Frames with no readable capture time among timed siblings. They form
    /// their own group and never join a timed one -- on the reference data
    /// that is `Mantis_Stacked.tif`, a result file sitting beside its own
    /// sources.
    Untimed,
}

impl GroupSignal {
    /// The word the printed proposal uses.
    pub fn label(self) -> &'static str {
        match self {
            GroupSignal::Directory => "directory",
            GroupSignal::CaptureTime => "capture-time",
            GroupSignal::Both => "directory+capture-time",
            GroupSignal::Untimed => "untimed",
        }
    }
}

/// One frame's `DateTimeOriginal`, as seconds and as the text it was read
/// from.
///
/// `text` is kept verbatim because the printed proposal shows the boundary
/// times, and a re-rendered timestamp would be a second spelling of the same
/// fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTime {
    pub epoch_seconds: i64,
    pub text: String,
}

/// One proposed stack.
#[derive(Debug, Clone)]
pub struct Group {
    /// Readable, derived, and unique across the proposal -- a group without
    /// a name cannot have an output path.
    pub name: String,
    /// The directory its frames were enumerated from.
    pub dir: PathBuf,
    /// In `frame_paths` order (bytewise by file name), always.
    pub frames: Vec<PathBuf>,
    pub signal: GroupSignal,
    pub first_time: Option<CaptureTime>,
    pub last_time: Option<CaptureTime>,
}

/// What [`propose`] proposes: an ordered list of named groups plus the
/// threshold that produced the boundaries between them.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub root: PathBuf,
    pub groups: Vec<Group>,
    pub gap: Duration,
    /// `true` when at least one frame carried a readable capture time. When
    /// `false` the whole proposal rests on directory structure alone, and
    /// the printed proposal says so rather than leaving the reader to infer
    /// it from the absence of times.
    pub time_signal_available: bool,
}

/// Propose a grouping of every frame under `root`.
///
/// Header tags and directory entries only -- never a tile, never a pixel.
///
/// Walks depth-first, calling [`frame_paths`] at each directory: its
/// `NoFrames` is the **negative answer** ("no stack here"), so a thumbnails
/// or export subdirectory is skipped rather than aborting the walk. Any
/// other [`TiffError`] does abort. `frame_paths` also redirects a directory
/// holding a `frames/` subdirectory to that subdirectory, so the committed
/// fixture layout proposes one group for `fixtures/mantis-512`, not two.
///
/// Within one directory, frames are ordered by `(epoch_seconds, file_name)`
/// to find the boundaries -- the file-name tiebreak is load-bearing, since
/// most real intra-stack gaps are 0 s at second resolution and a sort keyed
/// on time alone would leave the order of most frames to the sort's tie
/// behaviour -- and then re-sorted by file name inside each group, restoring
/// exactly `frame_paths`' own order.
///
/// A frame whose capture time is absent, unparseable or whose header cannot
/// be read at all is **untimed**: it groups by directory instead, and says
/// so through [`GroupSignal`]. A `root` with no frame-bearing directory
/// anywhere below it is `Err(TiffError::NoFrames)`, the same refusal a plain
/// single-stack run over an empty directory already produces.
pub fn propose(root: &Path) -> Result<Proposal, TiffError> {
    let mut dirs = Vec::new();
    collect_frame_dirs(root, 0, &mut dirs)?;

    let mut groups = Vec::new();
    let mut time_signal_available = false;
    for (dir, frames) in dirs {
        let timed: Vec<Option<CaptureTime>> = frames.iter().map(|p| read_capture_time(p)).collect();
        if timed.iter().any(Option::is_some) {
            time_signal_available = true;
        }
        groups.extend(group_one_directory(&dir, &frames, &timed));
    }

    if groups.is_empty() {
        return Err(TiffError::NoFrames {
            path: root.to_path_buf(),
        });
    }

    assign_names(&mut groups);

    Ok(Proposal {
        root: root.to_path_buf(),
        groups,
        gap: GROUP_GAP,
        time_signal_available,
    })
}

/// Depth-first, bytewise by directory name, so a proposal is deterministic.
/// A directory that `frame_paths` resolved through its own `frames/`
/// subdirectory does not also get that subdirectory walked -- otherwise the
/// fixture layout would propose the same frames twice.
fn collect_frame_dirs(
    dir: &Path,
    depth: usize,
    out: &mut Vec<(PathBuf, Vec<PathBuf>)>,
) -> Result<(), TiffError> {
    if depth > MAX_DEPTH {
        return Ok(());
    }

    match frame_paths(dir) {
        Ok(frames) => {
            let self_contained = dir.join("frames").is_dir();
            out.push((dir.to_path_buf(), frames));
            // A directory holding a `frames/` subdirectory **is** one stack
            // -- that is what `frame_paths` means by redirecting into it --
            // so its other subdirectories are that stack's own baggage
            // (`golden/`, `export/`), never further stacks. Walking them
            // would propose `fixtures/mantis-512/golden` as a second group
            // and break the single-stack equivalence property outright.
            if self_contained {
                return Ok(());
            }
        }
        // "Not a stack here", not a failure: the walk continues.
        Err(TiffError::NoFrames { .. }) => {}
        Err(err) => return Err(err),
    }

    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        // A directory that cannot be listed at the top level is a real
        // failure; deeper down it was already listed to be found.
        Err(source) => {
            return Err(TiffError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut subdirs: Vec<PathBuf> = read_dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    for subdir in subdirs {
        collect_frame_dirs(&subdir, depth + 1, out)?;
    }
    Ok(())
}

/// Header-only read of one frame's capture time. A frame whose header will
/// not parse at all is untimed rather than fatal: grouping is not the
/// validator, and the run over that group reports the real error when it
/// opens the stack.
fn read_capture_time(path: &Path) -> Option<CaptureTime> {
    let header = TiffHeader::open(path).ok()?;
    parse_exif_datetime(header.date_time_original.as_deref()?)
}

/// Strict `"YYYY:MM:DD HH:MM:SS"` -- the exact 19-byte form the reference
/// frames carry -- to seconds since the Unix epoch, `None` on any deviation.
///
/// No date crate: the only operation the rule needs is a difference in
/// seconds, and `None` is already a first-class outcome. Timestamps are compared as
/// naive local time; `OffsetTimeOriginal` (36880) is ignored because a
/// constant offset cancels inside a difference.
pub fn parse_exif_datetime(text: &str) -> Option<CaptureTime> {
    let bytes = text.as_bytes();
    if bytes.len() != 19 {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        let ok = match i {
            4 | 7 => *b == b':',
            10 => *b == b' ',
            13 | 16 => *b == b':',
            _ => b.is_ascii_digit(),
        };
        if !ok {
            return None;
        }
    }
    let num = |from: usize, to: usize| -> i64 { text[from..to].parse::<i64>().unwrap_or(0) };
    let (year, month, day) = (num(0, 4), num(5, 7), num(8, 10));
    let (hour, minute, second) = (num(11, 13), num(14, 16), num(17, 19));
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(CaptureTime {
        epoch_seconds: days * 86_400 + hour * 3_600 + minute * 60 + second,
        text: text.to_string(),
    })
}

/// Days since 1970-01-01 for a proleptic Gregorian date -- Howard Hinnant's
/// `days_from_civil`, the standard branch-free form.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// One directory's frames, cut into groups by the gap rule. Names are left
/// empty here and assigned across the whole proposal by [`assign_names`],
/// which is the only place that can see a collision.
fn group_one_directory(
    dir: &Path,
    frames: &[PathBuf],
    times: &[Option<CaptureTime>],
) -> Vec<Group> {
    let mut timed: Vec<(&PathBuf, &CaptureTime)> = frames
        .iter()
        .zip(times)
        .filter_map(|(p, t)| t.as_ref().map(|t| (p, t)))
        .collect();
    let untimed: Vec<PathBuf> = frames
        .iter()
        .zip(times)
        .filter(|(_, t)| t.is_none())
        .map(|(p, _)| p.clone())
        .collect();

    if timed.is_empty() {
        return vec![Group {
            name: String::new(),
            dir: dir.to_path_buf(),
            frames: frames.to_vec(),
            signal: GroupSignal::Directory,
            first_time: None,
            last_time: None,
        }];
    }

    timed.sort_by(|a, b| {
        a.1.epoch_seconds
            .cmp(&b.1.epoch_seconds)
            .then_with(|| a.0.file_name().cmp(&b.0.file_name()))
    });

    let gap = GROUP_GAP.as_secs() as i64;
    let mut chunks: Vec<Vec<(&PathBuf, &CaptureTime)>> = vec![Vec::new()];
    for (i, entry) in timed.iter().enumerate() {
        if i > 0 {
            let previous = timed[i - 1].1.epoch_seconds;
            if entry.1.epoch_seconds - previous > gap {
                chunks.push(Vec::new());
            }
        }
        chunks
            .last_mut()
            .expect("chunks is never empty")
            .push(*entry);
    }

    // Both signals agree exactly when the directory resolves to one group:
    // one timed chunk and nothing untimed beside it.
    let signal = if chunks.len() == 1 && untimed.is_empty() {
        GroupSignal::Both
    } else {
        GroupSignal::CaptureTime
    };

    let mut groups: Vec<Group> = chunks
        .into_iter()
        .map(|chunk| {
            let first_time = chunk.first().map(|(_, t)| (*t).clone());
            let last_time = chunk.last().map(|(_, t)| (*t).clone());
            let mut frames: Vec<PathBuf> = chunk.into_iter().map(|(p, _)| p.clone()).collect();
            frames.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
            Group {
                name: String::new(),
                dir: dir.to_path_buf(),
                frames,
                signal,
                first_time,
                last_time,
            }
        })
        .collect();

    if !untimed.is_empty() {
        groups.push(Group {
            name: String::new(),
            dir: dir.to_path_buf(),
            frames: untimed,
            signal: GroupSignal::Untimed,
            first_time: None,
            last_time: None,
        });
    }

    groups
}

/// Names every group, then disambiguates, in three ordered rules: a
/// basename range; on collision the directory's basename in front; on a
/// remaining collision a `-2`, `-3` suffix. All three are visible in the
/// printed proposal, so no disambiguation is silent.
fn assign_names(groups: &mut [Group]) {
    for group in groups.iter_mut() {
        group.name = basename_range(&group.frames, &group.dir);
    }

    let colliding: Vec<String> = duplicates(groups);
    for group in groups.iter_mut() {
        if colliding.contains(&group.name) {
            let dir_name = dir_basename(&group.dir);
            if !dir_name.is_empty() && !group.name.starts_with(&format!("{dir_name}-")) {
                group.name = format!("{dir_name}-{}", group.name);
            }
        }
    }

    let colliding: Vec<String> = duplicates(groups);
    let mut seen: Vec<String> = Vec::new();
    for group in groups.iter_mut() {
        if colliding.contains(&group.name) {
            let base = group.name.clone();
            let mut n = 1;
            let mut candidate = base.clone();
            while seen.contains(&candidate) {
                n += 1;
                candidate = format!("{base}-{n}");
            }
            group.name = candidate;
        }
        seen.push(group.name.clone());
    }
}

fn duplicates(groups: &[Group]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, group) in groups.iter().enumerate() {
        if groups
            .iter()
            .enumerate()
            .any(|(j, other)| j != i && other.name == group.name)
            && !out.contains(&group.name)
        {
            out.push(group.name.clone());
        }
    }
    out
}

/// `20260829-IMG_9800` … `20260829-IMG_9860` -> `20260829-IMG_9800-9860`.
/// A one-frame group is its stem. Stems sharing no prefix at all -- the
/// mixed-camera case -- fall back to the directory's own basename, since a
/// name glued from two unrelated strings is worse than no range.
fn basename_range(frames: &[PathBuf], dir: &Path) -> String {
    let stem = |p: &PathBuf| -> String {
        p.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let Some(first) = frames.first().map(&stem) else {
        return dir_basename(dir);
    };
    let last = frames.last().map(&stem).unwrap_or_default();
    if frames.len() == 1 || first == last {
        return first;
    }
    let prefix_len = first
        .bytes()
        .zip(last.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    if prefix_len == 0 {
        return dir_basename(dir);
    }
    // The common prefix must not cut a *number* in half:
    // `20260829-IMG_9800` and `20260829-IMG_9860` share `20260829-IMG_98`,
    // and naming the range `20260829-IMG_9800-60` reads as nonsense. Back
    // the prefix out of the digit run so the range reads
    // `20260829-IMG_9800-9860` instead.
    let bytes = last.as_bytes();
    let mut prefix_len = prefix_len;
    if bytes.get(prefix_len).is_some_and(u8::is_ascii_digit) {
        while prefix_len > 0 && bytes[prefix_len - 1].is_ascii_digit() {
            prefix_len -= 1;
        }
    }
    if prefix_len == 0 {
        return dir_basename(dir);
    }
    // Byte-count prefixes can land inside a multi-byte character; only a
    // char boundary is a legal split.
    let prefix_len = (0..=prefix_len)
        .rev()
        .find(|n| last.is_char_boundary(*n))
        .unwrap_or(0);
    let suffix = &last[prefix_len..];
    if suffix.is_empty() {
        first
    } else {
        format!("{first}-{suffix}")
    }
}

fn dir_basename(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_exact_reference_form() {
        let parsed = parse_exif_datetime("2026:08:29 18:42:41").expect("the reference form parses");
        assert_eq!(parsed.text, "2026:08:29 18:42:41");
        // 2026-08-29 18:42:41 UTC, cross-checked against
        // `date -u -d '2026-08-29 18:42:41' +%s`.
        assert_eq!(parsed.epoch_seconds, 1_788_028_961);
    }

    #[test]
    fn a_one_second_difference_is_one_second() {
        let a = parse_exif_datetime("2026:08:29 18:42:41").unwrap();
        let b = parse_exif_datetime("2026:08:29 18:42:43").unwrap();
        assert_eq!(b.epoch_seconds - a.epoch_seconds, 2);
    }

    #[test]
    fn refuses_a_truncated_or_non_numeric_timestamp() {
        assert_eq!(parse_exif_datetime("2026:08:29 18:42"), None);
        assert_eq!(parse_exif_datetime("2026:08:29 18:42:4"), None);
        assert_eq!(parse_exif_datetime("2026:08:29T18:42:41"), None);
        assert_eq!(parse_exif_datetime("20xx:08:29 18:42:41"), None);
        assert_eq!(parse_exif_datetime(""), None);
        assert_eq!(parse_exif_datetime("2026:13:29 18:42:41"), None);
    }

    /// The `(epoch_seconds, file_name)` sort key's second component is
    /// load-bearing: 58 of the reference stack's 60 inter-frame gaps are
    /// exactly 0 s, so a key on time alone would leave the order of most
    /// frames to the sort's tie behaviour.
    #[test]
    fn frames_with_identical_times_order_by_file_name() {
        let dir = Path::new("/card");
        let frames: Vec<PathBuf> = ["c.tif", "a.tif", "b.tif"]
            .iter()
            .map(|n| dir.join(n))
            .collect();
        let same = parse_exif_datetime("2026:08:29 18:42:41");
        let times = vec![same.clone(), same.clone(), same];
        let groups = group_one_directory(dir, &frames, &times);
        assert_eq!(groups.len(), 1);
        let names: Vec<String> = groups[0]
            .frames
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.tif", "b.tif", "c.tif"]);
    }

    #[test]
    fn a_basename_range_reads_as_a_range() {
        let dir = Path::new("/card");
        let frames: Vec<PathBuf> = ["20260829-IMG_9800.tif", "20260829-IMG_9860.tif"]
            .iter()
            .map(|n| dir.join(n))
            .collect();
        assert_eq!(basename_range(&frames, dir), "20260829-IMG_9800-9860");
    }

    #[test]
    fn stems_with_no_common_prefix_fall_back_to_the_directory() {
        let dir = Path::new("/card/subject-a");
        let frames: Vec<PathBuf> = ["alpha.tif", "zulu.tif"]
            .iter()
            .map(|n| dir.join(n))
            .collect();
        assert_eq!(basename_range(&frames, dir), "subject-a");
    }
}
