// SPDX-License-Identifier: GPL-3.0-or-later

//! The algorithm registry: how a [`crate::algorithm::StackAlgorithm`] or
//! [`crate::algorithm::AlignAlgorithm`] becomes visible to `focusforge-cli`
//! and, later, the GUI, without either crate maintaining a list.
//!
//! # How registration works, and where it must live
//!
//! Registration is `inventory` (0.3.24): an algorithm's own file ends in one
//! `inventory::submit! { StackRegistration::new(&MyAlgo) }` line, and that
//! entry is collected process-wide at **link** time, not at any later
//! runtime step. Concretely, `submit!` expands to a `static` holding the
//! value plus an `unsafe extern "C" fn` constructor placed in `.init_array`
//! (Unix) / `.CRT$XCU` (Windows), run before `main`
//! (<https://github.com/dtolnay/inventory/blob/master/src/lib.rs>). A
//! registration in a compilation unit that nothing else references can
//! therefore be dropped by the linker — which is why every registration in
//! this crate must live in a module that is **compiled into the rlib
//! unconditionally**, never behind `#[cfg(test)]`. `#[cfg(test)]` code never
//! reaches the rlib at all, so a downstream crate's tests would never see
//! such a registration; that is exactly the failure `crate::test_algorithms`
//! (gated behind the `test-algorithms` feature instead, plain backticks here
//! since that module does not exist without it, and this doc is rendered
//! without `--all-features`) exists to rule out, and
//! `crates/cli/tests/registry_linkage.rs` proves it end to end.
//!
//! Ids are unique across **both** families: [`find`] takes a bare id with no
//! family qualifier, so a stacking and an alignment algorithm may never
//! share one. A collision is programmer error, not a runtime condition to
//! recover from, so it is a panic naming the colliding id rather than a
//! `Result` every caller has to thread through — see `index_or_panic`.
//!
//! # Platform note
//!
//! `inventory`'s constructor mechanism is platform-sensitive by nature; only
//! Linux is exercised in this repository.
//! [dtolnay/inventory#85](https://github.com/dtolnay/inventory/issues/85) is
//! open against Windows MSVC with recent rustc failing to link at all.

mod error;

use std::collections::BTreeMap;
use std::sync::OnceLock;

pub use error::{DuplicateId, Family, LookupError};

use crate::algorithm::{AlignAlgorithm, StackAlgorithm};
use crate::params::ParamDesc;

/// One stacking algorithm's entry in the process-wide `inventory` registry.
/// `algorithm` is private and `new` is the only constructor, so a
/// registration stays one obvious call and the struct's own representation
/// is free to change later.
pub struct StackRegistration {
    algorithm: &'static dyn StackAlgorithm,
}

impl StackRegistration {
    pub const fn new(algorithm: &'static dyn StackAlgorithm) -> Self {
        StackRegistration { algorithm }
    }
}

inventory::collect!(StackRegistration);

/// One alignment algorithm's entry in the process-wide `inventory` registry.
/// Twin of [`StackRegistration`] for the other family.
pub struct AlignRegistration {
    algorithm: &'static dyn AlignAlgorithm,
}

impl AlignRegistration {
    pub const fn new(algorithm: &'static dyn AlignAlgorithm) -> Self {
        AlignRegistration { algorithm }
    }
}

inventory::collect!(AlignRegistration);

/// A family-neutral handle onto one registered algorithm, returned by every
/// listing so a caller need not match on family before printing an id, a
/// display name or a schema.
#[derive(Clone, Copy)]
pub enum AlgorithmRef {
    Stack(&'static dyn StackAlgorithm),
    Align(&'static dyn AlignAlgorithm),
}

impl std::fmt::Debug for AlgorithmRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple(match self {
            AlgorithmRef::Stack(_) => "Stack",
            AlgorithmRef::Align(_) => "Align",
        })
        .field(&self.id())
        .finish()
    }
}

impl AlgorithmRef {
    pub fn id(&self) -> &'static str {
        match self {
            AlgorithmRef::Stack(a) => a.id(),
            AlgorithmRef::Align(a) => a.id(),
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            AlgorithmRef::Stack(a) => a.display_name(),
            AlgorithmRef::Align(a) => a.display_name(),
        }
    }

    pub fn params(&self) -> &'static [ParamDesc] {
        match self {
            AlgorithmRef::Stack(a) => a.params(),
            AlgorithmRef::Align(a) => a.params(),
        }
    }

    pub fn family(&self) -> Family {
        match self {
            AlgorithmRef::Stack(_) => Family::Stack,
            AlgorithmRef::Align(_) => Family::Align,
        }
    }

    pub fn test_only(&self) -> bool {
        match self {
            AlgorithmRef::Stack(a) => a.test_only(),
            AlgorithmRef::Align(a) => a.test_only(),
        }
    }

    pub fn is_default(&self) -> bool {
        match self {
            AlgorithmRef::Stack(a) => a.is_default(),
            AlgorithmRef::Align(a) => a.is_default(),
        }
    }

    /// Whether this algorithm is a specialist choice: front ends list it after
    /// the others. Alignment algorithms are never advanced as a whole; their
    /// parameters carry the flag.
    pub fn advanced(&self) -> bool {
        match self {
            AlgorithmRef::Stack(a) => a.advanced(),
            AlgorithmRef::Align(_) => false,
        }
    }
}

/// The algorithm of `family` to use when a caller names none: among the
/// entries that are not test-only, the one marked [`AlgorithmRef::is_default`],
/// else the sole entry. Several unmarked entries, or several marked ones,
/// are refused naming the candidates; id order never settles it. Pure, like
/// [`index`].
fn default_in<I>(entries: I, family: Family) -> Result<AlgorithmRef, LookupError>
where
    I: IntoIterator<Item = AlgorithmRef>,
{
    let real: Vec<AlgorithmRef> = entries
        .into_iter()
        .filter(|e| e.family() == family && !e.test_only())
        .collect();
    let marked: Vec<AlgorithmRef> = real.iter().copied().filter(|e| e.is_default()).collect();
    let candidates = match (marked.as_slice(), real.as_slice()) {
        ([one], _) => return Ok(*one),
        ([], [only]) => return Ok(*only),
        ([], all) => all,
        (several, _) => several,
    };
    Err(LookupError::NoDefault {
        family,
        candidates: candidates.iter().map(|e| e.id().to_string()).collect(),
    })
}

/// Builds an id-keyed index over `entries`, failing on the first id claimed
/// twice. Pure: touches no process-wide state, which is what lets a test
/// feed it a synthetic duplicate without poisoning `inventory`'s real
/// registry for the rest of the binary.
fn index<I>(entries: I) -> Result<BTreeMap<&'static str, AlgorithmRef>, DuplicateId>
where
    I: IntoIterator<Item = AlgorithmRef>,
{
    let mut map: BTreeMap<&'static str, AlgorithmRef> = BTreeMap::new();
    for entry in entries {
        let id = entry.id();
        if let Some(existing) = map.get(id) {
            return Err(DuplicateId {
                id: id.to_string(),
                first: existing.family(),
                second: entry.family(),
            });
        }
        map.insert(id, entry);
    }
    Ok(map)
}

/// [`index`], panicking with [`DuplicateId`]'s own message on collision.
/// Generic over the same iterator [`index`] is, which is what lets a
/// `#[should_panic]` test cover this exact panic message on synthetic
/// entries — a real duplicate registration cannot be compiled in without
/// poisoning `inventory`'s process-wide static for every other test in the
/// binary.
fn index_or_panic<I>(entries: I) -> BTreeMap<&'static str, AlgorithmRef>
where
    I: IntoIterator<Item = AlgorithmRef>,
{
    match index(entries) {
        Ok(map) => map,
        Err(dup) => panic!("{dup}"),
    }
}

static INDEX: OnceLock<BTreeMap<&'static str, AlgorithmRef>> = OnceLock::new();

/// The real, process-wide index: every `StackRegistration` and
/// `AlignRegistration` `inventory` has collected, merged and panicking on a
/// collision. Built once, on first use.
fn index_of_registrations() -> &'static BTreeMap<&'static str, AlgorithmRef> {
    INDEX.get_or_init(|| {
        let stacks = inventory::iter::<StackRegistration>
            .into_iter()
            .map(|r| AlgorithmRef::Stack(r.algorithm));
        let aligns = inventory::iter::<AlignRegistration>
            .into_iter()
            .map(|r| AlgorithmRef::Align(r.algorithm));
        index_or_panic(stacks.chain(aligns))
    })
}

/// Every registered algorithm of either family, by id.
pub fn algorithms() -> Vec<AlgorithmRef> {
    index_of_registrations().values().copied().collect()
}

/// Every registered stacking algorithm, by id.
pub fn stack_algorithms() -> Vec<&'static dyn StackAlgorithm> {
    index_of_registrations()
        .values()
        .filter_map(|entry| match entry {
            AlgorithmRef::Stack(a) => Some(*a),
            AlgorithmRef::Align(_) => None,
        })
        .collect()
}

/// Every registered alignment algorithm, by id.
pub fn align_algorithms() -> Vec<&'static dyn AlignAlgorithm> {
    index_of_registrations()
        .values()
        .filter_map(|entry| match entry {
            AlgorithmRef::Align(a) => Some(*a),
            AlgorithmRef::Stack(_) => None,
        })
        .collect()
}

fn known_ids() -> Vec<String> {
    index_of_registrations()
        .keys()
        .map(|id| id.to_string())
        .collect()
}

/// Resolves a bare id, in either family. `Unknown` only when the id is in
/// neither family; an id that exists in the other family is never reported
/// as unknown.
pub fn find(id: &str) -> Result<AlgorithmRef, LookupError> {
    index_of_registrations()
        .get(id)
        .copied()
        .ok_or_else(|| LookupError::Unknown {
            id: id.to_string(),
            known: known_ids(),
        })
}

/// Resolves `id` as a stacking algorithm. An id registered as an alignment
/// algorithm is [`LookupError::WrongFamily`], never [`LookupError::Unknown`].
pub fn find_stack(id: &str) -> Result<&'static dyn StackAlgorithm, LookupError> {
    match find(id)? {
        AlgorithmRef::Stack(a) => Ok(a),
        AlgorithmRef::Align(_) => Err(LookupError::WrongFamily {
            id: id.to_string(),
            actual: Family::Align,
            wanted: Family::Stack,
        }),
    }
}

/// Resolves `id` as an alignment algorithm. An id registered as a stacking
/// algorithm is [`LookupError::WrongFamily`], never [`LookupError::Unknown`].
pub fn find_align(id: &str) -> Result<&'static dyn AlignAlgorithm, LookupError> {
    match find(id)? {
        AlgorithmRef::Align(a) => Ok(a),
        AlgorithmRef::Stack(_) => Err(LookupError::WrongFamily {
            id: id.to_string(),
            actual: Family::Stack,
            wanted: Family::Align,
        }),
    }
}

/// The stacking algorithm to use when the caller names none; the rule is
/// [`default_align`]'s.
pub fn default_stack() -> Result<&'static dyn StackAlgorithm, LookupError> {
    match default_in(index_of_registrations().values().copied(), Family::Stack)? {
        AlgorithmRef::Stack(a) => Ok(a),
        AlgorithmRef::Align(_) => unreachable!("default_in keeps only the requested family"),
    }
}

/// The alignment algorithm to use when the caller names none: the one
/// non-test-only registration marked default, else the sole non-test-only
/// one; otherwise [`LookupError::NoDefault`].
pub fn default_align() -> Result<&'static dyn AlignAlgorithm, LookupError> {
    match default_in(index_of_registrations().values().copied(), Family::Align)? {
        AlgorithmRef::Align(a) => Ok(a),
        AlgorithmRef::Stack(_) => unreachable!("default_in keeps only the requested family"),
    }
}

// Tests live in a sibling file, not inline: see `tests.rs`'s module doc for
// why -- in short, so this file (the facade) never mentions a dummy type or
// a dummy id, which a grep-based test checks this exact file for.
#[cfg(test)]
mod tests;
