// SPDX-License-Identifier: GPL-3.0-or-later

//! Additive `ImageDescription` tokens the aligned `stack` path carries on
//! the composite: which align algorithm ran, the anchor frame, its mode,
//! and the crop rect -- reported under **both** `--no-crop` and the
//! default, since crop is applied by default; `cropped=` is what tells
//! the two apart.
//!
//! Grammar: `FocusForge <version> align=<id> anchor=<index> mode=<id>
//! [crop=<x>,<y>,<w>,<h> cropped=<bool>]`. Unknown `key=value` tokens are
//! ignored. `TiffWriter`'s default description is
//! `"FocusForge <version>"` (`crate::io::tiff::writer`) -- passing a
//! `description` replaces that default, so this module re-emits the same
//! prefix itself rather than dropping it.
//!
//! The Z-Map's own `FocusForge zmap v1 frames=<N>` description is
//! untouched: these tokens live on the composite only, and
//! [`crate::zmap::parse_description`] keeps refusing this string as
//! [`crate::zmap::ZMapError::NotAZMap`], because its three-token prefix
//! (`FocusForge <version>`, not `FocusForge zmap v1`) never matches.

use crate::tile::Rect;

/// `FocusForge <version> align=<algorithm-id> anchor=<index> mode=<mode-id>`,
/// plus `crop=<x>,<y>,<w>,<h> cropped=<bool>` when `crop` is `Some((rect,
/// applied))`. Omitted only when no crop rect exists at all (the frames'
/// footprints do not intersect) -- `applied` alone, not the presence of
/// the tokens, is what distinguishes `--no-crop` from the default.
pub fn format_composite_description(
    align_algorithm: &str,
    anchor: usize,
    mode: &str,
    crop: Option<(Rect, bool)>,
) -> String {
    let mut s = format!(
        "FocusForge {} align={align_algorithm} anchor={anchor} mode={mode}",
        env!("CARGO_PKG_VERSION")
    );
    if let Some((rect, applied)) = crop {
        s.push_str(&format!(
            " crop={},{},{},{} cropped={applied}",
            rect.x, rect.y, rect.width, rect.height
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Test-only round trip: splits every whitespace-separated `key=value`
    /// token, ignoring the non-`key=value` version prefix -- proves the
    /// grammar is genuinely parseable, without adding a public parser.
    fn tokens(description: &str) -> HashMap<&str, &str> {
        description
            .split_ascii_whitespace()
            .filter_map(|t| t.split_once('='))
            .collect()
    }

    #[test]
    fn format_round_trips_every_token() {
        let rect = Rect {
            x: 31,
            y: 40,
            width: 422,
            height: 424,
        };
        let s =
            format_composite_description("pyramidal-ecc", 3, "rotation-scale", Some((rect, true)));

        assert!(s.starts_with(&format!("FocusForge {}", env!("CARGO_PKG_VERSION"))));
        let tokens = tokens(&s);
        assert_eq!(tokens["align"], "pyramidal-ecc");
        assert_eq!(tokens["anchor"], "3");
        assert_eq!(tokens["mode"], "rotation-scale");
        assert_eq!(tokens["crop"], "31,40,422,424");
        assert_eq!(tokens["cropped"], "true");
    }

    #[test]
    fn crop_tokens_are_omitted_when_none() {
        let s = format_composite_description("pyramidal-ecc", 0, "translation", None);
        assert!(!s.contains("crop="));
        assert!(!s.contains("cropped="));
        let tokens = tokens(&s);
        assert_eq!(tokens["align"], "pyramidal-ecc");
        assert_eq!(tokens["mode"], "translation");
    }

    #[test]
    fn cropped_false_is_distinguishable_from_omitted() {
        let rect = Rect {
            x: 2,
            y: 2,
            width: 10,
            height: 10,
        };
        let s =
            format_composite_description("pyramidal-ecc", 0, "translation", Some((rect, false)));
        assert_eq!(tokens(&s)["cropped"], "false");
    }

    #[test]
    fn composite_string_is_still_refused_as_a_zmap() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let s =
            format_composite_description("pyramidal-ecc", 0, "translation", Some((rect, false)));
        let err = crate::zmap::parse_description(std::path::Path::new("x.tif"), Some(&s))
            .expect_err("a composite description must never parse as a Z-Map");
        assert!(matches!(err, crate::zmap::ZMapError::NotAZMap { .. }));
    }
}
