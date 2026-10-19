// SPDX-License-Identifier: GPL-3.0-or-later

//! A suffixed, human-readable byte-size parser: the one syntax of the
//! `--max-host-memory` and `--max-vram` flags and of the size strings in the
//! settings file. Hand-written rather than a crate dependency: ~30 lines plus
//! tests is smaller than the licence and `deny.toml` review a new dependency
//! would need.

const ACCEPTED: &str =
    "accepted suffixes: B, KB/KiB, MB/MiB, GB/GiB, TB/TiB (or no suffix for bytes)";

/// Parses a decimal integer, optional whitespace, and an optional
/// case-insensitive suffix: `B`; `KiB`/`MiB`/`GiB`/`TiB` (1024-based);
/// `KB`/`MB`/`GB`/`TB` (1000-based). No suffix means bytes.
///
/// Refuses, each with a message naming the accepted suffixes: a value that
/// does not start with a decimal integer (a bare negative sign, or a leading
/// fraction like `1.5GiB`), a leading integer too large for `u64`, an
/// unrecognized suffix, and a byte count that overflows `u64` once the
/// suffix's multiplier is applied.
pub fn parse_byte_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits_end == 0 {
        return Err(format!(
            "{s:?} does not start with a decimal integer; {ACCEPTED}"
        ));
    }
    let (digits, rest) = s.split_at(digits_end);
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("{digits:?} is not a valid u64 integer (overflow?); {ACCEPTED}"))?;

    let suffix = rest.trim();
    let multiplier: u64 = match suffix.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KIB" => 1 << 10,
        "MIB" => 1 << 20,
        "GIB" => 1 << 30,
        "TIB" => 1 << 40,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        other => return Err(format!("unknown size suffix {other:?}; {ACCEPTED}")),
    };

    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{s:?} overflows u64 bytes; {ACCEPTED}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_binary_decimal_bare_and_spaced_forms() {
        for (input, expected) in [
            ("8GiB", 8u64 << 30),
            ("512MiB", 512u64 << 20),
            ("1000MB", 1_000_000_000),
            ("4096", 4096),
            ("1 GiB", 1u64 << 30),
        ] {
            assert_eq!(parse_byte_size(input), Ok(expected), "input {input:?}");
        }
    }

    #[test]
    fn refuses_malformed_sizes_naming_the_accepted_suffixes() {
        for input in [
            "8Gigs",
            "8G",
            "-1",
            "1.5GiB",
            "99999999999999999999",
            "18446744073709551615GiB",
        ] {
            let err = parse_byte_size(input).unwrap_err();
            assert!(err.contains("GiB"), "input {input:?}: {err}");
        }
    }
}
