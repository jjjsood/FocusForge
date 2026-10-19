// SPDX-License-Identifier: GPL-3.0-or-later

//! The pinned, authoritative `ImageDescription` grammar a Z-Map's frame
//! count lives in:
//!
//! ```text
//! FocusForge zmap v1 frames=<N>
//! ```
//!
//! The three-token `FocusForge zmap v1` prefix is not redundant:
//! `TiffWriter::new` writes `"FocusForge <version>"` when `TiffSpec::description`
//! is `None` (`crate::io::tiff::writer`), so an ordinary composite carries a
//! description whose second token is a version number -- the three-token
//! prefix separates the two unambiguously, and a composite opened as a
//! Z-Map is [`super::ZMapError::NotAZMap`]. Unknown `key=value` tokens are
//! ignored.

use std::path::Path;

use super::ZMapError;

/// Produce `FocusForge zmap v1 frames=<frame_count>` exactly.
pub fn format_description(frame_count: u32) -> String {
    format!("FocusForge zmap v1 frames={frame_count}")
}

/// Parse a Z-Map's frame count out of its `ImageDescription`.
///
/// `None` is [`ZMapError::MissingDescription`]; the first three
/// whitespace-separated tokens must be exactly `FocusForge`, `zmap`, `v1`,
/// else [`ZMapError::NotAZMap`]; the remaining tokens are `key=value`, of
/// which `frames` is required ([`ZMapError::MissingFrameCount`]) and must
/// parse as a `u32` within `1..=super::MAX_FRAMES`
/// ([`ZMapError::BadFrameCount`]). Unknown keys are ignored.
pub fn parse_description(path: &Path, description: Option<&str>) -> Result<u32, ZMapError> {
    let description = description.ok_or_else(|| ZMapError::MissingDescription {
        path: path.to_path_buf(),
    })?;

    let mut tokens = description.split_ascii_whitespace();
    let prefix_ok = tokens.next() == Some("FocusForge")
        && tokens.next() == Some("zmap")
        && tokens.next() == Some("v1");
    if !prefix_ok {
        return Err(ZMapError::NotAZMap {
            path: path.to_path_buf(),
            description: description.to_string(),
        });
    }

    let mut frames_token = None;
    for token in tokens {
        if let Some(value) = token.strip_prefix("frames=") {
            frames_token = Some(value);
        }
        // Unknown keys are ignored on purpose.
    }

    let token = frames_token.ok_or_else(|| ZMapError::MissingFrameCount {
        path: path.to_path_buf(),
        description: description.to_string(),
    })?;

    let frame_count: u32 = token.parse().map_err(|_| ZMapError::BadFrameCount {
        path: path.to_path_buf(),
        token: token.to_string(),
    })?;

    if frame_count == 0 || frame_count > super::MAX_FRAMES {
        return Err(ZMapError::BadFrameCount {
            path: path.to_path_buf(),
            token: token.to_string(),
        });
    }

    Ok(frame_count)
}
