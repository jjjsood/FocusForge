// SPDX-License-Identifier: GPL-3.0-or-later

//! Failures the [`super`] registry can report: a lookup that names an id
//! nothing registered, or in the wrong family, and a collision between two
//! registrations that share an id.
//!
//! Modelled on `crate::params::error`: every variant names the offending
//! value, and the unknown-id case names what *was* allowed, not only what
//! was not (`ParamViolation::UnknownKey`).

use thiserror::Error;

/// Which of the two algorithm families a registration or a lookup belongs
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Stack,
    Align,
}

impl std::fmt::Display for Family {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Family::Stack => "stacking",
            Family::Align => "alignment",
        };
        f.write_str(s)
    }
}

/// Two registrations claimed the same id, across either family.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "duplicate algorithm id {id:?}: registered as a {first} algorithm and again as a {second} algorithm"
)]
pub struct DuplicateId {
    pub id: String,
    pub first: Family,
    pub second: Family,
}

/// A lookup by id failed: either nothing registered that id, or it exists
/// but in the other family than the one the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LookupError {
    /// No registered algorithm, in either family, uses this id.
    #[error("unknown algorithm {id:?}, known algorithms: {}", known.join(", "))]
    Unknown { id: String, known: Vec<String> },

    /// The id resolves, but to an algorithm of the other family.
    // Phrased without an article on purpose: `actual` and `wanted` are
    // family names ("stacking", "alignment"), and "a alignment" is what a
    // naive `a {actual}` produces.
    #[error("{id:?} belongs to the {actual} family, not the {wanted} family")]
    WrongFamily {
        id: String,
        actual: Family,
        wanted: Family,
    },

    /// No id was given and the family has no single default: `candidates`
    /// is empty when nothing non-test-only is registered, otherwise it names
    /// every algorithm the caller could pick from.
    #[error("{}", no_default_message(*family, candidates))]
    NoDefault {
        family: Family,
        candidates: Vec<String>,
    },
}

fn no_default_message(family: Family, candidates: &[String]) -> String {
    if candidates.is_empty() {
        format!("no {family} algorithm is registered")
    } else {
        format!(
            "multiple {family} algorithms are registered; pass an id to choose one: {}",
            candidates.join(", ")
        )
    }
}
