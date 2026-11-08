// SPDX-License-Identifier: GPL-3.0-or-later

//! Decides whether a newer final release than the running one exists.
//!
//! The release answer arrives through [`ReleaseSource`]; the check itself does
//! no network I/O. [`HttpReleaseSource`] is the HTTPS source.

mod http;

use semver::Version;

pub use http::{HttpReleaseSource, RELEASE_URL};

/// Supplies the body of the latest-release answer (one GitHub release object).
pub trait ReleaseSource {
    /// The answer body, or a printable message describing why it could not be
    /// obtained.
    fn fetch(&self) -> Result<String, String>;
}

/// Outcome of one update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    /// The running version is the newest final release or newer, or the newest
    /// release is the skipped one.
    UpToDate,
    /// A newer final release exists.
    Newer {
        /// Version of the newer release.
        version: Version,
        /// Download page of the release.
        url: String,
    },
    /// The check could not be completed.
    Failed(CheckFailure),
}

/// Why a check could not be completed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckFailure {
    /// The source could not deliver an answer.
    #[error("could not reach the release server: {0}")]
    Source(String),
    /// The answer was not a usable release object.
    #[error("the release answer could not be read: {0}")]
    Unreadable(String),
    /// The answer describes a draft or pre-release, not a final release.
    #[error("no final release is published")]
    NoFinalRelease,
}

/// Parses a release tag or version string, accepting one leading `v`.
pub fn parse_version(tag: &str) -> Result<Version, semver::Error> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
}

/// Compares the latest release from `source` with the `running` version.
///
/// `skipped`, when it parses, suppresses the release with that version
/// (compared by precedence). `running` must be a valid version.
pub fn check(source: &dyn ReleaseSource, running: &str, skipped: Option<&str>) -> UpdateCheck {
    check_inner(source, running, skipped).unwrap_or_else(UpdateCheck::Failed)
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    prerelease: bool,
    draft: bool,
}

fn check_inner(
    source: &dyn ReleaseSource,
    running: &str,
    skipped: Option<&str>,
) -> Result<UpdateCheck, CheckFailure> {
    let body = source.fetch().map_err(CheckFailure::Source)?;
    let release: Release =
        serde_json::from_str(&body).map_err(|e| CheckFailure::Unreadable(e.to_string()))?;
    if release.draft || release.prerelease {
        return Err(CheckFailure::NoFinalRelease);
    }
    let version = parse_version(&release.tag_name).map_err(|e| {
        CheckFailure::Unreadable(format!("tag {:?} is not a version: {e}", release.tag_name))
    })?;
    if !version.pre.is_empty() {
        return Err(CheckFailure::NoFinalRelease);
    }
    if !release.html_url.starts_with("https://") {
        return Err(CheckFailure::Unreadable(format!(
            "download link {:?} is not an https URL",
            release.html_url
        )));
    }

    let running =
        parse_version(running).expect("the running version is the crate's semver version");
    let is_skipped = skipped
        .and_then(|s| parse_version(s).ok())
        .is_some_and(|s| s.cmp_precedence(&version).is_eq());
    if version.cmp_precedence(&running).is_gt() && !is_skipped {
        Ok(UpdateCheck::Newer {
            version,
            url: release.html_url,
        })
    } else {
        Ok(UpdateCheck::UpToDate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Canned(Result<String, String>);

    impl ReleaseSource for Canned {
        fn fetch(&self) -> Result<String, String> {
            self.0.clone()
        }
    }

    fn release(tag: &str, prerelease: bool, draft: bool) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/o/r/releases/tag/{tag}","prerelease":{prerelease},"draft":{draft},"id":1}}"#
        )
    }

    fn run(body: &str, running: &str, skipped: Option<&str>) -> UpdateCheck {
        check(&Canned(Ok(body.to_owned())), running, skipped)
    }

    fn newer(version: &str, tag: &str) -> UpdateCheck {
        UpdateCheck::Newer {
            version: Version::parse(version).unwrap(),
            url: format!("https://github.com/o/r/releases/tag/{tag}"),
        }
    }

    #[test]
    fn newer_final_release_is_reported() {
        for (running, tag, version) in [
            ("0.1.0", "v0.2.0", "0.2.0"),
            ("0.1.0", "0.2.0", "0.2.0"),
            ("0.2.0-rc.1", "v0.2.0", "0.2.0"),
            ("0.1.0", "v1.0.0", "1.0.0"),
        ] {
            assert_eq!(
                run(&release(tag, false, false), running, None),
                newer(version, tag),
                "running {running}, tag {tag}"
            );
        }
    }

    #[test]
    fn equal_or_older_release_is_up_to_date() {
        for (running, tag) in [
            ("0.2.0", "v0.2.0"),
            ("0.2.0+abc", "v0.2.0"),
            ("0.2.0", "v0.2.0+def"),
            ("0.3.0", "v0.2.0"),
            ("0.10.0", "v0.9.0"),
        ] {
            assert_eq!(
                run(&release(tag, false, false), running, None),
                UpdateCheck::UpToDate,
                "running {running}, tag {tag}"
            );
        }
    }

    #[test]
    fn prereleases_are_ignored() {
        for body in [
            release("v9.0.0", false, true),
            release("v9.0.0", true, false),
            release("v9.0.0-rc.1", false, false),
        ] {
            assert_eq!(
                run(&body, "0.1.0", None),
                UpdateCheck::Failed(CheckFailure::NoFinalRelease),
                "{body}"
            );
        }
    }

    #[test]
    fn skipped_version_suppresses_only_that_version() {
        let body = release("0.2.0", false, false);
        for skipped in ["v0.2.0", "0.2.0"] {
            assert_eq!(
                run(&body, "0.1.0", Some(skipped)),
                UpdateCheck::UpToDate,
                "skipped {skipped}"
            );
        }
        assert_eq!(
            run(&release("v0.3.0", false, false), "0.1.0", Some("0.2.0")),
            newer("0.3.0", "v0.3.0")
        );
        assert_eq!(
            run(&body, "0.1.0", Some("garbage")),
            newer("0.2.0", "0.2.0")
        );
    }

    #[test]
    fn unreadable_answers_fail_with_a_reason() {
        let source_error = check(&Canned(Err("timed out".into())), "0.1.0", None);
        assert_eq!(
            source_error,
            UpdateCheck::Failed(CheckFailure::Source("timed out".into()))
        );

        let truncated = &release("v0.2.0", false, false)[..40];
        for body in [
            "",
            truncated,
            "[]",
            r#"{"message":"Not Found"}"#,
            r#"{"tag_name":"v0.2.0","prerelease":false,"draft":false}"#,
            &release("latest", false, false),
            &release("v1.2", false, false),
            r#"{"tag_name":"v0.2.0","html_url":"http://example.org/r","prerelease":false,"draft":false}"#,
        ] {
            assert!(
                matches!(
                    run(body, "0.1.0", None),
                    UpdateCheck::Failed(CheckFailure::Unreadable(_))
                ),
                "{body}"
            );
        }
    }
}
