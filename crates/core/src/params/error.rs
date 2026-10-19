// SPDX-License-Identifier: GPL-3.0-or-later

//! Validation and parse failures for [`crate::params`].
//!
//! Modelled on `crate::io::error`: [`ParamViolation`] is the data-carrying
//! inner type, one variant per rejection case, and [`ParamError`] is what a
//! fallible operation actually returns — an aggregate, because a schema
//! violation is reported for *every* offending parameter in one pass, not
//! only the first.

use thiserror::Error;

/// One rejected parameter: names the parameter id and the offending value,
/// plus whatever the schema would have allowed.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ParamViolation {
    /// A numeric value fell outside its descriptor's declared `min`/`max`.
    #[error("{id}: value {value} is outside the allowed range [{min}, {max}]")]
    OutOfRange {
        id: String,
        value: f64,
        min: f64,
        max: f64,
    },

    /// An `Enum` value named a variant its descriptor does not declare.
    #[error("{id}: unknown variant {value:?}, allowed: {}", allowed.join(", "))]
    UnknownVariant {
        id: String,
        value: String,
        allowed: Vec<String>,
    },

    /// A value's kind does not match its descriptor's declared kind, and is
    /// never coerced across the mismatch.
    #[error("{id}: expected {expected}, found {found}")]
    TypeMismatch {
        id: String,
        expected: &'static str,
        found: &'static str,
    },

    /// A key present in the values but absent from the schema.
    #[error("unknown parameter {key:?}, declared parameters: {}", declared.join(", "))]
    UnknownKey { key: String, declared: Vec<String> },

    /// A `Float` value that is `NaN` or infinite. `serde_json` silently
    /// serializes such a value to `null`, which then deserializes to
    /// nothing at all, so it must never enter a resolved or validated set.
    #[error("{id}: value {value} is not finite")]
    NotFinite { id: String, value: f64 },

    /// Text handed to [`super::ParamValue::parse`] does not parse as its
    /// descriptor's declared kind.
    #[error("{id}: {text:?} does not parse as {kind}")]
    Unparsable {
        id: String,
        kind: &'static str,
        text: String,
    },

    /// An `auto` hook produced a value that itself fails validation against
    /// its own descriptor.
    #[error("{id}: auto value rejected: {source}")]
    Auto {
        id: String,
        #[source]
        source: Box<ParamViolation>,
    },
}

/// Every violation found while validating or resolving one set of values
/// against a schema. Reports every offender in one pass — a user fixing a
/// command line is not sent round the loop once per mistake.
#[derive(Debug, Clone, PartialEq, Error)]
#[error("{}", .violations.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))]
pub struct ParamError {
    violations: Vec<ParamViolation>,
}

impl ParamError {
    pub(crate) fn new(violations: Vec<ParamViolation>) -> Self {
        Self { violations }
    }

    /// The individual violations, in the order they were found.
    pub fn violations(&self) -> &[ParamViolation] {
        &self.violations
    }

    /// Consumes the error, returning the individual violations.
    pub fn into_violations(self) -> Vec<ParamViolation> {
        self.violations
    }
}
