// SPDX-License-Identifier: GPL-3.0-or-later

//! The grouping rule: both signals in isolation, their precedence, the
//! degraded inputs, and the naming.
//!
//! Every tree here is **built by the test**: no committed fixture frame
//! carries a capture-time tag and `TiffWriter` cannot write one, so a test
//! that copied fixtures would prove nothing about the time signal. The
//! frames come from `focusforge_core::io::test_frames`, behind the
//! `test-algorithms` feature this crate's own dev-dependency enables.

mod common;
#[path = "common/raw_dng.rs"]
mod raw_dng;

use std::path::{Path, PathBuf};

use focusforge_core::io::test_frames::{
    write_frame_with_capture_time, write_frame_with_only_tag_306,
};
use focusforge_core::io::{GROUP_GAP, GroupSignal, TiffError, propose};
use raw_dng::{DngSpec, write_dng};

fn tree(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp tree must be creatable");
    dir
}

/// One 8x4 frame named `name` in `dir`, carrying `when` as
/// `DateTimeOriginal` (or no date tag at all when `when` is `None`).
fn frame(dir: &Path, name: &str, when: Option<&str>) {
    std::fs::create_dir_all(dir).expect("frame directory must be creatable");
    write_frame_with_capture_time(&dir.join(name), 8, 4, when).expect("test frame must write");
}

fn raw_frame(dir: &Path, name: &str, when: Option<&str>) {
    std::fs::create_dir_all(dir).expect("RAW frame directory must be creatable");
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]);
    spec.capture_time = when.map(str::to_string);
    let source = write_dng(&spec, true);
    std::fs::copy(source.path(), dir.join(name)).expect("RAW frame must copy");
}

fn raw_frame_with_modify_time(dir: &Path, name: &str, original: Option<&str>, modified: &str) {
    std::fs::create_dir_all(dir).expect("RAW frame directory must be creatable");
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]);
    spec.capture_time = original.map(str::to_string);
    spec.modify_time = Some(modified.to_string());
    let source = write_dng(&spec, true);
    std::fs::copy(source.path(), dir.join(name)).expect("RAW frame must copy");
}

fn names(group: &focusforge_core::io::Group) -> Vec<String> {
    group
        .frames
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn flat_directory_splits_on_the_capture_time_gap() {
    let dir = tree("grouping_flat_directory");
    for (name, when) in [
        ("IMG_9801.tif", "2026:08:29 18:42:41"),
        ("IMG_9802.tif", "2026:08:29 18:42:42"),
        ("IMG_9803.tif", "2026:08:29 18:42:43"),
        ("IMG_9910.tif", "2026:08:29 19:02:55"),
        ("IMG_9911.tif", "2026:08:29 19:02:56"),
    ] {
        frame(&dir, name, Some(when));
    }

    let proposal = propose(&dir).expect("a flat card dump must propose");
    assert_eq!(proposal.groups.len(), 2, "two subjects, two groups");
    assert_eq!(proposal.gap, GROUP_GAP);
    assert_eq!(
        names(&proposal.groups[0]),
        vec!["IMG_9801.tif", "IMG_9802.tif", "IMG_9803.tif"]
    );
    assert_eq!(
        names(&proposal.groups[1]),
        vec!["IMG_9910.tif", "IMG_9911.tif"]
    );
    assert_eq!(proposal.groups[0].signal, GroupSignal::CaptureTime);
    assert_eq!(
        proposal.groups[0].first_time.as_ref().unwrap().text,
        "2026:08:29 18:42:41"
    );
    assert_eq!(
        proposal.groups[0].last_time.as_ref().unwrap().text,
        "2026:08:29 18:42:43"
    );
}

#[test]
fn raw_frames_group_by_capture_time() {
    let dir = tree("grouping_raw_capture_time");
    raw_frame(&dir, "IMG_0001.dng", Some("2026:08:29 18:42:41"));
    raw_frame(&dir, "IMG_0002.dng", Some("2026:08:29 18:42:42"));
    raw_frame(&dir, "IMG_0010.dng", Some("2026:08:29 19:02:55"));

    let proposal = propose(&dir).expect("RAW frames must propose");
    assert_eq!(proposal.groups.len(), 2);
    assert_eq!(
        names(&proposal.groups[0]),
        vec!["IMG_0001.dng", "IMG_0002.dng"]
    );
    assert_eq!(names(&proposal.groups[1]), vec!["IMG_0010.dng"]);
}

#[test]
fn mixed_formats_partition_before_capture_time() {
    let dir = tree("grouping_mixed_formats");
    raw_frame(&dir, "raw.dng", Some("2026:08:29 18:42:41"));
    frame(&dir, "tiff.tif", Some("2026:08:29 18:42:41"));

    let proposal = propose(&dir).expect("mixed directory must propose partitions");
    assert_eq!(proposal.groups.len(), 2);
    assert_eq!(proposal.groups[0].signal, GroupSignal::RawBoth);
    assert_eq!(proposal.groups[1].signal, GroupSignal::TiffBoth);
    assert_eq!(
        proposal.groups[0].signal.label(),
        "raw/directory+capture-time"
    );
    assert_eq!(
        proposal.groups[1].signal.label(),
        "tiff/directory+capture-time"
    );
    assert_ne!(proposal.groups[0].name, proposal.groups[1].name);
}

#[test]
fn directory_structure_produces_one_group_per_frame_bearing_directory() {
    let root = tree("grouping_directory_structure");
    // Both subjects are contiguous in time -- one second apart across the
    // directory boundary -- so only the directory signal can separate them.
    frame(
        &root.join("subject-a"),
        "00.tif",
        Some("2026:08:29 18:42:41"),
    );
    frame(
        &root.join("subject-a"),
        "01.tif",
        Some("2026:08:29 18:42:42"),
    );
    frame(
        &root.join("subject-b"),
        "00.tif",
        Some("2026:08:29 18:42:43"),
    );
    frame(
        &root.join("subject-b"),
        "01.tif",
        Some("2026:08:29 18:42:44"),
    );

    let proposal = propose(&root).expect("a sorted tree must propose");
    assert_eq!(proposal.groups.len(), 2, "one group per directory");
    assert_eq!(proposal.groups[0].dir, root.join("subject-a"));
    assert_eq!(proposal.groups[1].dir, root.join("subject-b"));
    for group in &proposal.groups {
        assert_eq!(group.signal, GroupSignal::Both);
        assert_eq!(group.frames.len(), 2);
    }
}

/// Precedence holds in **both** directions: a capture-time gap inside one
/// directory still subdivides it, and a contiguous-in-time pair of
/// directories is never joined.
#[test]
fn directories_partition_before_capture_time_subdivides() {
    let root = tree("grouping_precedence");
    frame(&root.join("a"), "00.tif", Some("2026:08:29 18:42:41"));
    frame(&root.join("a"), "01.tif", Some("2026:08:29 18:52:41"));
    frame(&root.join("b"), "00.tif", Some("2026:08:29 18:42:42"));

    let proposal = propose(&root).expect("the tree must propose");
    assert_eq!(
        proposal.groups.len(),
        3,
        "`a` subdivides on its ten-minute gap; `b` is never joined to `a`"
    );
    assert_eq!(proposal.groups[0].dir, root.join("a"));
    assert_eq!(proposal.groups[1].dir, root.join("a"));
    assert_eq!(proposal.groups[2].dir, root.join("b"));
    assert_eq!(names(&proposal.groups[0]), vec!["00.tif"]);
    assert_eq!(names(&proposal.groups[1]), vec!["01.tif"]);
}

#[test]
fn missing_capture_time_falls_back_and_reports_it() {
    let dir = tree("grouping_missing_capture_time");
    frame(&dir, "00.tif", None);
    frame(&dir, "01.tif", Some("not-a-timestamp"));
    frame(&dir, "02.tif", None);

    let proposal = propose(&dir).expect("untimed frames must still group");
    assert_eq!(proposal.groups.len(), 1, "directory structure alone");
    assert_eq!(proposal.groups[0].signal, GroupSignal::Directory);
    assert_eq!(proposal.groups[0].frames.len(), 3);
    assert!(
        !proposal.time_signal_available,
        "the proposal must say the time signal was unavailable"
    );
}

#[test]
fn raw_missing_or_malformed_original_time_ignores_modify_time() {
    let dir = tree("grouping_raw_missing_original_time");
    raw_frame_with_modify_time(&dir, "00.dng", None, "2026:09:11 23:20:52");
    raw_frame_with_modify_time(
        &dir,
        "01.dng",
        Some("not-a-timestamp"),
        "2026:09:11 23:20:53",
    );

    let proposal = propose(&dir).expect("untimed RAW frames must still group");
    assert_eq!(proposal.groups.len(), 1);
    assert_eq!(proposal.groups[0].signal, GroupSignal::Directory);
    assert_eq!(names(&proposal.groups[0]), ["00.dng", "01.dng"]);
    assert!(!proposal.time_signal_available);
}

#[test]
fn an_untimed_frame_never_joins_a_timed_group() {
    let dir = tree("grouping_untimed_sibling");
    frame(&dir, "00.tif", Some("2026:08:29 18:42:41"));
    frame(&dir, "01.tif", Some("2026:08:29 18:42:42"));
    frame(&dir, "result.tif", None);

    let proposal = propose(&dir).expect("the directory must propose");
    assert_eq!(proposal.groups.len(), 2);
    assert_eq!(proposal.groups[0].signal, GroupSignal::CaptureTime);
    assert_eq!(names(&proposal.groups[0]), vec!["00.tif", "01.tif"]);
    assert_eq!(proposal.groups[1].signal, GroupSignal::Untimed);
    assert_eq!(names(&proposal.groups[1]), vec!["result.tif"]);
    assert!(proposal.time_signal_available);
}

/// `DateTime` (306) is darktable's export time, not capture time, and is
/// never consulted: a frame carrying only 306 groups as untimed.
#[test]
fn tag_306_does_not_group() {
    let dir = tree("grouping_tag_306");
    frame(&dir, "00.tif", Some("2026:08:29 18:42:41"));
    write_frame_with_only_tag_306(&dir.join("01.tif"), 8, 4, "2026:09:11 23:20:52")
        .expect("test frame must write");

    let proposal = propose(&dir).expect("the directory must propose");
    assert_eq!(proposal.groups.len(), 2);
    assert_eq!(proposal.groups[1].signal, GroupSignal::Untimed);
    assert_eq!(names(&proposal.groups[1]), vec!["01.tif"]);
}

#[test]
fn no_frames_in_a_subdirectory_is_a_negative_not_an_error() {
    let root = tree("grouping_empty_subdirectory");
    frame(&root.join("subject"), "00.tif", Some("2026:08:29 18:42:41"));
    std::fs::create_dir_all(root.join("thumbnails")).unwrap();
    std::fs::write(root.join("thumbnails").join("readme.txt"), b"not a frame").unwrap();
    std::fs::create_dir_all(root.join("export")).unwrap();

    let proposal = propose(&root).expect("a frameless subdirectory must not abort the walk");
    assert_eq!(proposal.groups.len(), 1);
    assert_eq!(proposal.groups[0].dir, root.join("subject"));
    let mut skipped = proposal.skipped.clone();
    skipped.sort();
    assert_eq!(
        skipped,
        vec![root.join("export"), root.join("thumbnails")],
        "both frameless subdirectories must be reported, not swallowed"
    );
}

/// The progress-observing walk reports the same [`focusforge_core::io::Proposal`]
/// `propose` does and counts every frame exactly once, on a tree with two
/// frame-bearing subfolders and a skipped one.
#[test]
fn progress_observing_walk_matches_propose_and_counts_every_frame_once() {
    use focusforge_core::io::propose_with_progress;

    let root = tree("grouping_progress");
    frame(
        &root.join("subject-a"),
        "00.tif",
        Some("2026:08:29 18:42:41"),
    );
    frame(
        &root.join("subject-a"),
        "01.tif",
        Some("2026:08:29 18:42:42"),
    );
    frame(
        &root.join("subject-b"),
        "00.tif",
        Some("2026:08:29 18:42:43"),
    );
    std::fs::create_dir_all(root.join("thumbnails")).unwrap();
    std::fs::write(root.join("thumbnails").join("readme.txt"), b"not a frame").unwrap();

    let reference = propose(&root).expect("the tree must propose");

    let mut ticks: Vec<(usize, usize)> = Vec::new();
    let observed = propose_with_progress(&root, |frames_read, frame_total| {
        ticks.push((frames_read, frame_total));
    })
    .expect("the observed walk must propose the same tree");

    assert_eq!(observed.root, reference.root);
    assert_eq!(observed.gap, reference.gap);
    assert_eq!(
        observed.time_signal_available,
        reference.time_signal_available
    );
    let mut observed_skipped = observed.skipped.clone();
    observed_skipped.sort();
    let mut reference_skipped = reference.skipped.clone();
    reference_skipped.sort();
    assert_eq!(observed_skipped, reference_skipped);
    assert_eq!(observed.groups.len(), reference.groups.len());
    for (o, r) in observed.groups.iter().zip(reference.groups.iter()) {
        assert_eq!(o.name, r.name);
        assert_eq!(o.dir, r.dir);
        assert_eq!(o.frames, r.frames);
        assert_eq!(o.signal, r.signal);
    }

    // Three real frames across the tree -- the skipped `thumbnails`
    // subdirectory holds none, so it contributes no report.
    assert_eq!(
        ticks.len(),
        3,
        "one report per frame read, not per group or per directory: {ticks:?}"
    );
    let frame_total = 3;
    for (i, &(frames_read, total)) in ticks.iter().enumerate() {
        assert_eq!(
            total, frame_total,
            "frame_total must stay fixed across every report"
        );
        assert_eq!(
            frames_read,
            i + 1,
            "frames_read must count up by exactly one per report, never skip or repeat"
        );
    }
}

#[test]
fn an_empty_tree_is_no_frames() {
    let root = tree("grouping_empty_tree");
    std::fs::create_dir_all(root.join("nothing")).unwrap();
    match propose(&root) {
        Err(TiffError::NoFrames { path }) => assert_eq!(path, root),
        other => panic!("expected NoFrames, got {other:?}"),
    }
}

#[test]
fn group_names_are_derived_readably() {
    let dir = tree("grouping_names");
    for (name, when) in [
        ("20260829-IMG_9800.tif", "2026:08:29 18:42:41"),
        ("20260829-IMG_9830.tif", "2026:08:29 18:42:42"),
        ("20260829-IMG_9860.tif", "2026:08:29 18:42:43"),
    ] {
        frame(&dir, name, Some(when));
    }
    let proposal = propose(&dir).expect("the directory must propose");
    assert_eq!(proposal.groups.len(), 1);
    assert_eq!(proposal.groups[0].name, "20260829-IMG_9800-9860");

    // A group from a directory whose stems share no prefix falls back to
    // the directory's own name.
    let mixed = tree("grouping_names_mixed");
    frame(&mixed.join("subject-a"), "alpha.tif", None);
    frame(&mixed.join("subject-a"), "zulu.tif", None);
    let proposal = propose(&mixed).expect("the tree must propose");
    assert_eq!(proposal.groups[0].name, "subject-a");
}

#[test]
fn duplicate_group_names_are_disambiguated() {
    let root = tree("grouping_duplicate_names");
    // Same file names in two directories: rule 1 gives both the same name.
    frame(&root.join("a"), "00.tif", Some("2026:08:29 18:42:41"));
    frame(&root.join("b"), "00.tif", Some("2026:08:29 18:42:41"));
    // And two time-split groups inside one directory whose single frames
    // would otherwise both be "solo".
    frame(&root.join("c"), "solo.tif", Some("2026:08:29 18:42:41"));
    frame(&root.join("c"), "solo2.tif", Some("2026:08:29 19:42:41"));

    let proposal = propose(&root).expect("the tree must propose");
    let all: Vec<&str> = proposal.groups.iter().map(|g| g.name.as_str()).collect();
    let mut unique = all.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        all.len(),
        unique.len(),
        "no two groups may share a name -- a name becomes an output path: {all:?}"
    );
    assert!(
        all.contains(&"a-00") && all.contains(&"b-00"),
        "rule 2 prefixes the directory basename: {all:?}"
    );
}

/// The signature carries no CLI type -- `Path`, `PathBuf`, `Duration`,
/// `String` and this crate's own `TiffError` only. This test *is* the check:
/// it links `focusforge-core` alone, the same call a GUI consumer makes.
#[test]
fn grouping_is_callable_without_cli_types() {
    let dir = tree("grouping_library_call");
    frame(&dir, "00.tif", Some("2026:08:29 18:42:41"));
    let proposal: focusforge_core::io::Proposal = propose(dir.as_path()).expect("must propose");
    let _: &Path = proposal.root.as_path();
    let _: std::time::Duration = proposal.gap;
    let _: &str = proposal.groups[0].name.as_str();
    let _: &[PathBuf] = &proposal.groups[0].frames;
}

/// The whole rule reads header tags and directory entries. Nothing here
/// opens a `TiffStack`, reads a tile or touches a pixel -- a boundary kept
/// checkable by a source-level grep as well as by this test's own cost.
#[test]
fn proposing_reads_no_pixels() {
    let dir = tree("grouping_no_pixels");
    frame(&dir, "00.tif", Some("2026:08:29 18:42:41"));
    let start = std::time::Instant::now();
    let proposal = propose(&dir).expect("must propose");
    assert_eq!(proposal.groups.len(), 1);
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

/// `propose()` over the real reference stack. The expected count depends on
/// the directory's layout: with a `frames/` subdirectory it is one
/// self-contained stack (**one** group); flat, it is **two** groups, the 61
/// timed frames plus `Mantis_Stacked.tif` alone as untimed. The test detects
/// the layout and asserts that answer. It reads at most 62 headers.
#[test]
#[ignore = "reads the 16 GB reference stack"]
fn proposal_over_the_reference_stack() {
    let Some(dir) = common::reference_stack_dir() else {
        return;
    };
    let self_contained = dir.join("frames").is_dir();
    let start = std::time::Instant::now();
    let proposal = propose(&dir).expect("the reference stack must propose");
    eprintln!(
        "proposal_over_the_reference_stack: {} groups in {:?} (reference-stack frames present: {})",
        proposal.groups.len(),
        start.elapsed(),
        self_contained,
    );
    for group in &proposal.groups {
        eprintln!(
            "  {} {} frames {:?}",
            group.name,
            group.frames.len(),
            group.signal
        );
    }

    assert_eq!(proposal.groups[0].frames.len(), 61);
    assert!(matches!(
        proposal.groups[0].signal,
        GroupSignal::CaptureTime | GroupSignal::Both
    ));
    assert!(proposal.groups[0].name.contains("20260829-IMG_98"));

    if self_contained {
        assert_eq!(
            proposal.groups.len(),
            1,
            "the reference stack's frames/ subdirectory makes it self-contained: \
             Mantis_Stacked.tif must not be walked"
        );
    } else {
        assert_eq!(proposal.groups.len(), 2);
        assert_eq!(proposal.groups[1].signal, GroupSignal::Untimed);
        assert_eq!(names(&proposal.groups[1]), vec!["Mantis_Stacked.tif"]);
    }
}
