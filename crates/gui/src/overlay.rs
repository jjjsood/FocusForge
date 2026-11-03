// SPDX-License-Identifier: GPL-3.0-or-later

//! The projects overlay's own plain-Rust vocabulary: what a card that never
//! opened stands for, and the arithmetic laying every card out on a grid.
//!
//! Plain Rust, no `use slint::` -- `crate::bridge` is what turns a
//! [`CardView`](crate::overlay::CardView) into the Slint struct the overlay
//! actually renders.

use std::path::{Path, PathBuf};

use focusforge_core::display::{self, Downsampler};
use focusforge_core::io::TiffStack;
use focusforge_core::project::Plane;
use focusforge_core::tile::Rect;

use crate::entry::GuiProject;

/// The four ways a stack can fail to become an open project. Each carries
/// its own fixed headline, tone and action -- a fixed copy, not derived
/// from any error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Failed,
    Cancelled,
    SourcesMissing,
    NotAProject,
}

impl FailureKind {
    /// `FFProjectCard`'s own `kind` string for this failure.
    pub fn card_kind(self) -> &'static str {
        match self {
            FailureKind::Failed => "failed",
            FailureKind::Cancelled => "cancelled",
            FailureKind::SourcesMissing => "sources-missing",
            FailureKind::NotAProject => "not-a-project",
        }
    }

    pub fn headline(self) -> &'static str {
        match self {
            FailureKind::Failed => "Did not stack",
            FailureKind::Cancelled => "Cancelled",
            FailureKind::SourcesMissing => "Sources missing",
            FailureKind::NotAProject => "Not a project",
        }
    }

    /// Empty for [`FailureKind::NotAProject`], the one kind with no action.
    pub fn action_label(self) -> &'static str {
        match self {
            FailureKind::Failed | FailureKind::Cancelled => "Run again",
            FailureKind::SourcesMissing => "Locate frames…",
            FailureKind::NotAProject => "",
        }
    }
}

/// The `Sources missing` card's reason and the missing-sources dialog's
/// first body sentence -- one counted string, built once, read by both.
pub fn sources_missing_sentence(missing: usize, total: usize) -> String {
    format!(
        "A project remembers its frames by path. {missing} of {total} cannot be found in this location."
    )
}

/// The missing-sources dialog's own title.
pub const SOURCES_MISSING_TITLE: &str = "The frames are no longer there";

/// The missing-sources dialog's full body: [`sources_missing_sentence`]
/// plus what still works -- a fused checkpoint's stored composite still
/// shows without its sources, so the second paragraph never claims there
/// is nothing to show.
pub fn sources_missing_body(missing: usize, total: usize) -> String {
    format!(
        "{}\n\nHistory and Z-Map are complete. Fused results still show; edited checkpoints and \
         re-fusing need the frames.",
        sources_missing_sentence(missing, total)
    )
}

/// The fingerprint-mismatch dialog's own title.
pub const MISMATCH_TITLE: &str = "The source frames are not the same";

/// The fingerprint-mismatch dialog's full body: the recorded frame count
/// plus the fixed Z-Map-consequence paragraph.
pub fn mismatch_body(frame_count: usize) -> String {
    format!(
        "The project was made over {frame_count} frames. The frames in this location give a \
         different fingerprint.\n\nThe Z-Map records, for every pixel, which frame it came from \
         — by that frame's place in the row. If the row is wrong, every retouch points at the \
         wrong frame."
    )
}

/// One `Not a project` card: a path with no open project behind it at all.
/// `path` is what a later successful open of the same path clears the card
/// by.
pub struct FailureCard {
    pub kind: FailureKind,
    pub path: PathBuf,
    pub title: String,
    pub stage: String,
    pub reason: String,
    pub meta: String,
}

/// The plain-Rust mirror of `FFProjectCard`'s own properties, positioned on
/// the grid -- `crate::bridge` is the one place this crosses into Slint.
#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    /// "open" | "failed" | "cancelled" | "sources-missing" | "not-a-project".
    pub kind: String,
    /// Has an open project behind it -- true for every tab's own card, false
    /// for a `state.failures` entry with no open project.
    pub opened: bool,
    pub current: bool,
    pub unsaved: bool,
    pub fusing: bool,
    pub fusing_fraction: f32,
    pub project_name: String,
    pub meta_text: String,
    pub headline: String,
    pub stage: String,
    pub reason: String,
    pub action_label: String,
    /// `Some` once this project's own picture is primed -- shared rather
    /// than cloned per push, since it is read far more often than it
    /// changes.
    pub picture: Option<std::sync::Arc<Thumbnail>>,
    /// The card's own top-left corner, in the grid's own coordinate space --
    /// [`card_origin`]'s own units, before any gallery inset or scroll.
    pub x: f32,
    pub y: f32,
}

/// A card's fixed footprint and the gap between neighbours -- the fixed
/// card geometry every card, however many, is laid out against.
pub const CARD_WIDTH: f32 = 330.0;
pub const CARD_HEIGHT: f32 = 252.0;
pub const CARD_GAP: f32 = 24.0;
/// The gallery's own left inset, read back off the live document alongside
/// the card geometry above.
pub const GALLERY_INSET: f32 = 32.0;

/// How many cards fit side by side in `width` -- the gallery's own measured
/// content width, insets already subtracted by the caller's own layout.
/// `0` only when `width` cannot fit even one card.
pub fn columns(width: f32) -> usize {
    if width < CARD_WIDTH {
        return 0;
    }
    (((width - CARD_WIDTH) / (CARD_WIDTH + CARD_GAP)).floor() as usize) + 1
}

/// The scrollable content height `cards` cards need, wrapped at `columns`
/// per row. `0` with no cards or no room for even one column.
pub fn content_height(cards: usize, columns: usize) -> f32 {
    if cards == 0 || columns == 0 {
        return 0.0;
    }
    let rows = cards.div_ceil(columns);
    rows as f32 * CARD_HEIGHT + rows.saturating_sub(1) as f32 * CARD_GAP
}

/// `index`'s own `(x, y)` top-left corner within the grid, `columns` wide.
/// `(0.0, 0.0)` when `columns` is `0` -- nothing is laid out at all.
pub fn card_origin(index: usize, columns: usize) -> (f32, f32) {
    if columns == 0 {
        return (0.0, 0.0);
    }
    let col = index % columns;
    let row = index / columns;
    (
        col as f32 * (CARD_WIDTH + CARD_GAP),
        row as f32 * (CARD_HEIGHT + CARD_GAP),
    )
}

/// A card's picture size -- the 330 x 186 upper region, minus the tone bar.
pub const THUMBNAIL_WIDTH: u32 = 330;
pub const THUMBNAIL_HEIGHT: u32 = 186;

/// A card's picture, already tone-mapped to RGBA8 -- `crate::bridge` is the
/// one place this crosses into `slint::Image::from_rgba8`.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Tone-maps a finished [`Downsampler`] reduction into a [`Thumbnail`].
fn tonemap_thumbnail(samples: Vec<u16>, samples_per_pixel: u32) -> Thumbnail {
    let mut rgba = vec![0u8; THUMBNAIL_WIDTH as usize * THUMBNAIL_HEIGHT as usize * 4];
    display::tonemap_cpu(
        &samples,
        THUMBNAIL_WIDTH,
        THUMBNAIL_HEIGHT,
        samples_per_pixel,
        0,
        &mut rgba,
    );
    Thumbnail {
        width: THUMBNAIL_WIDTH,
        height: THUMBNAIL_HEIGHT,
        rgba,
    }
}

/// A sealed project's own stored composite, downsampled and tone-mapped --
/// one full zstd decompress + blake3 pass, band-streamed so the composite is
/// never held whole. `None` on any read failure (a corrupt or unreadable
/// container); the card simply carries no picture then.
fn render_thumbnail_from_project(
    project: &mut focusforge_core::project::Project,
) -> Option<Thumbnail> {
    let plane = project.plane(Plane::Composite).clone();
    let mut accumulator = Downsampler::new(
        plane.width,
        plane.height,
        plane.samples_per_pixel,
        THUMBNAIL_WIDTH,
        THUMBNAIL_HEIGHT,
    );
    project
        .read_plane_into(Plane::Composite, &mut |band, rect| {
            accumulator.add_band(band, rect);
        })
        .ok()?;
    Some(tonemap_thumbnail(
        accumulator.finish(),
        plane.samples_per_pixel,
    ))
}

/// The picture of the sealed project at `path`, opened on the calling
/// thread; `None` when it cannot be opened or read.
pub(crate) fn render_thumbnail_from_file(path: &Path) -> Option<Thumbnail> {
    let mut project = focusforge_core::project::Project::open(path).ok()?;
    render_thumbnail_from_project(&mut project)
}

/// A loose TIFF -- an unfused tab's first frame or a held composite, 16-bit or
/// 32-bit float -- downsampled and tone-mapped the same way, read band by band
/// via [`TiffStack::read_tile_into`], never as a whole image. `None` on any
/// read failure.
pub(crate) fn render_thumbnail_from_frame(first_frame: &Path) -> Option<Thumbnail> {
    let stack = TiffStack::open_staged(first_frame).ok()?;
    let (width, height) = stack.dimensions();
    let samples_per_pixel = stack.header(0).samples_per_pixel;
    let mut accumulator = Downsampler::new(
        width,
        height,
        samples_per_pixel,
        THUMBNAIL_WIDTH,
        THUMBNAIL_HEIGHT,
    );
    let crop = accumulator.crop();
    const BAND_ROWS: u32 = 64;
    let mut band =
        vec![0u16; crop.width as usize * BAND_ROWS as usize * samples_per_pixel as usize];
    let mut y = crop.y;
    while y < crop.y + crop.height {
        let rows = BAND_ROWS.min(crop.y + crop.height - y);
        let rect = Rect {
            x: crop.x,
            y,
            width: crop.width,
            height: rows,
        };
        let len = rect.width as usize * rect.height as usize * samples_per_pixel as usize;
        stack.read_tile_into(0, rect, &mut band[..len]).ok()?;
        accumulator.add_band(&band[..len], rect);
        y += rows;
    }
    Some(tonemap_thumbnail(accumulator.finish(), samples_per_pixel))
}

/// A card's picture for `project`: the stored composite for an opened one,
/// the first of `frames` for one still unfused. `frames` is the tab's own
/// ordered frame list ([`crate::entry::project_frames`]) -- empty only for
/// a frame-less group, which the entry path never actually produces.
pub fn render_thumbnail(project: &mut GuiProject, frames: &[PathBuf]) -> Option<Thumbnail> {
    match project {
        GuiProject::Opened(project) => render_thumbnail_from_project(project),
        GuiProject::Unsaved(_) => render_thumbnail_from_frame(frames.first()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusforge_core::io::{TiffSpec, TiffWriter};
    use focusforge_core::tile::Sample;

    const WIDTH: u32 = 48;
    const HEIGHT: u32 = 32;

    fn spec() -> TiffSpec {
        TiffSpec {
            width: WIDTH,
            height: HEIGHT,
            samples_per_pixel: 3,
            description: None,
        }
    }

    fn whole() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        }
    }

    /// A float composite and the 16-bit one holding the same samples clipped.
    #[test]
    fn a_float_composite_has_the_thumbnail_of_its_clipped_samples() {
        let dir = tempfile::tempdir().unwrap();
        let stored: Vec<f32> = (0..WIDTH * HEIGHT * 3)
            .map(|i| i as f32 * 20.0 - 3000.0)
            .collect();
        assert!(stored.iter().any(|&v| v < 0.0) && stored.iter().any(|&v| v > 65535.0));
        let float_path = dir.path().join("float.tif");
        let mut writer = TiffWriter::create_f32(&float_path, spec()).unwrap();
        writer.write_tile_f32(whole(), &stored).unwrap();
        writer.finish().unwrap();
        let clipped_path = dir.path().join("clipped.tif");
        let clipped: Vec<u16> = stored.iter().map(|&v| u16::narrow(v)).collect();
        let mut writer = TiffWriter::create(&clipped_path, spec()).unwrap();
        writer.write_tile(whole(), &clipped).unwrap();
        writer.finish().unwrap();

        let from_float =
            render_thumbnail_from_frame(&float_path).expect("a float composite has a thumbnail");
        assert_eq!(
            Some(from_float),
            render_thumbnail_from_frame(&clipped_path),
            "the float composite reads as its samples clipped to 16 bits"
        );
    }
}
