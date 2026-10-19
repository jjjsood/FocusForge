// SPDX-License-Identifier: GPL-3.0-or-later

//! Core-level export metadata tests: the metadata rewrite rule over a
//! project built from a synthetic source frame, plus a reference-stack
//! check of the metadata read itself.

mod common;

use std::path::PathBuf;

use focusforge_core::align::Registration;
use focusforge_core::io::test_frames::{
    FrameMetadata, ascii_entry, longs_bo, write_frame_with_metadata,
};
use focusforge_core::io::{ByteOrder, TiffSpec, TiffWriter, read_source_metadata};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::{ExportOptions, MetadataOptions};
use focusforge_core::project::{self, Metrics, Project, SealRequest};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

const WIDTH: u32 = 32;
const HEIGHT: u32 = 32;

fn full(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A one-frame project whose only source carries a MakerNote sentinel, a
/// `DocumentName` leaking a path, an `Interop` pointer, a dummy IFD1
/// ("thumbnail"), and enough whitelisted EXIF to make the EXIF switch carry
/// something at all.
fn seal_project_with_rich_source(name: &str) -> (PathBuf, String, PathBuf) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let frame_path = dir.join("source.tif");
    write_frame_with_metadata(
        &frame_path,
        WIDTH,
        HEIGHT,
        &FrameMetadata {
            ifd0: vec![
                (271, 2, ascii_entry("TestCam")),         // Make, whitelisted
                (37500, 7, MAKER_NOTE_SENTINEL.to_vec()), // MakerNote, dropped
                (269, 2, ascii_entry("/home/user/private/export.tif")), // DocumentName, dropped
            ],
            exif: vec![
                (36867, 2, ascii_entry("2024:03:17 08:00:00")), // DateTimeOriginal, kept
                (40965, 4, longs_bo(ByteOrder::Little, &[0])),  // Interop pointer, dropped
            ],
            thumbnail: true,
            ..Default::default()
        },
    )
    .expect("source frame must write");

    let composite_path = staging.join("composite.tif");
    let mut writer = TiffWriter::create(
        &composite_path,
        TiffSpec {
            width: WIDTH,
            height: HEIGHT,
            samples_per_pixel: 3,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer
        .write_tile(
            full(WIDTH, HEIGHT),
            &vec![0u16; (WIDTH * HEIGHT * 3) as usize],
        )
        .unwrap();
    writer.finish().unwrap();

    let zmap_path = staging.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap_path, ZMapInfo::new(WIDTH, HEIGHT, 1).unwrap()).unwrap();
    map.write_tile(full(WIDTH, HEIGHT), &vec![0u16; (WIDTH * HEIGHT) as usize])
        .unwrap();
    map.finish().unwrap();

    let project_path = dir.join(format!("rich.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &[frame_path],
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    (project_path, sealed.checkpoint, dir)
}

const MAKER_NOTE_SENTINEL: &[u8] = b"SENTINEL-MAKERNOTE-0xACAB1234";

#[test]
fn rewrites_dimensions_software_and_drops_layout_tags() {
    let (project_path, checkpoint, dir) = seal_project_with_rich_source("export_metadata_rewrite");
    let mut project = Project::open(&project_path).unwrap();

    let before = chrono::Local::now();
    let out_path = dir.join("out.tif");
    let outcome = project::export_checkpoint(
        &mut project,
        &checkpoint,
        &out_path,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect("export must succeed");
    let after = chrono::Local::now();

    // The sentinel MakerNote bytes must not appear anywhere in the output
    // file -- checked over the raw bytes, since the reader would already
    // filter it out even if the writer had (wrongly) carried it forward.
    let raw = std::fs::read(&out_path).unwrap();
    assert!(
        !raw.windows(MAKER_NOTE_SENTINEL.len())
            .any(|window| window == MAKER_NOTE_SENTINEL),
        "the exported file must never contain the source's MakerNote bytes"
    );

    // IFD0's next-IFD offset must be 0: the source's own IFD1 ("thumbnail")
    // must never survive into the export.
    let ifd0_at = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(raw[ifd0_at..ifd0_at + 2].try_into().unwrap()) as usize;
    let next_ifd_pos = ifd0_at + 2 + count * 12;
    assert_eq!(
        u32::from_le_bytes(raw[next_ifd_pos..next_ifd_pos + 4].try_into().unwrap()),
        0,
        "the export's IFD0 must have no next IFD"
    );

    let exported = read_source_metadata(&out_path);
    assert!(exported.warnings.is_empty(), "{:?}", exported.warnings);

    fn find(
        entries: &[focusforge_core::io::TagEntry],
        tag: u16,
    ) -> Option<&focusforge_core::io::TagEntry> {
        entries.iter().find(|e| e.tag == tag)
    }

    // Software (305).
    let software = find(&exported.ifd0, 305).expect("Software must be present");
    let software_text = String::from_utf8_lossy(&software.value);
    assert!(software_text.starts_with("FocusForge "), "{software_text}");

    // DateTime (306), the export's own local wall-clock moment.
    let datetime = find(&exported.ifd0, 306).expect("DateTime must be present");
    let datetime_text = String::from_utf8_lossy(&datetime.value)
        .trim_end_matches('\0')
        .to_string();
    let parsed = chrono::NaiveDateTime::parse_from_str(&datetime_text, "%Y:%m:%d %H:%M:%S")
        .expect("DateTime must parse");
    assert!(
        parsed >= before.naive_local() - chrono::Duration::seconds(2)
            && parsed <= after.naive_local() + chrono::Duration::seconds(2),
        "DateTime {datetime_text} must fall within the export's before/after window"
    );

    // DateTimeOriginal (36867) survives untouched.
    let original = find(&exported.exif, 36867).expect("DateTimeOriginal must survive");
    assert_eq!(
        String::from_utf8_lossy(&original.value).trim_end_matches('\0'),
        "2024:03:17 08:00:00"
    );

    // OffsetTime (36880) is present.
    assert!(
        find(&exported.exif, 36880).is_some(),
        "OffsetTime must be written"
    );

    // ExifImageWidth/Height (40962/40963) equal the export's own size.
    let width = find(&exported.exif, 40962).expect("ExifImageWidth must be present");
    assert_eq!(
        u32::from_le_bytes(width.value[..4].try_into().unwrap()),
        WIDTH
    );
    let height = find(&exported.exif, 40963).expect("ExifImageHeight must be present");
    assert_eq!(
        u32::from_le_bytes(height.value[..4].try_into().unwrap()),
        HEIGHT
    );

    // MakerNote (37500), DocumentName (269) and Interop (40965) never
    // survive into the export.
    for tag in [37500u16, 269, 40965] {
        assert!(
            find(&exported.ifd0, tag).is_none() && find(&exported.exif, tag).is_none(),
            "tag {tag} must not survive into the export"
        );
    }

    assert_eq!(outcome.frame, 0);
}

#[test]
#[ignore = "reads one reference-stack frame's header"]
fn reads_reference_frame_metadata() {
    let Some(stack_dir) = common::reference_stack_dir() else {
        return;
    };
    let frame_path = stack_dir.join("20260829-IMG_9800.tif");
    let metadata = read_source_metadata(&frame_path);

    assert!(
        !metadata.exif.is_empty(),
        "the reference frame's EXIF IFD must be found"
    );
    let icc = metadata
        .icc
        .expect("the reference frame's ICC profile must be found");
    assert_eq!(
        icc.len(),
        9012,
        "the reference frame's ICC profile is 9012 bytes"
    );
    assert!(
        metadata.xmp.as_ref().is_some_and(|xmp| !xmp.is_empty()),
        "the reference frame's XMP packet must be non-empty"
    );
    assert!(
        metadata.iptc.is_some(),
        "the reference frame's IPTC block must be found"
    );
    assert!(
        metadata.exif.iter().any(|e| e.tag == 36867),
        "DateTimeOriginal must be carried into the EXIF list, wherever IFD0 put it"
    );
    assert!(
        metadata.ifd0.iter().all(|e| e.tag != 269),
        "DocumentName, which carries an absolute export path, must never be carried"
    );
}
