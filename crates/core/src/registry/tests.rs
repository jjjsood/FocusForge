// SPDX-License-Identifier: GPL-3.0-or-later

//! Registry tests, kept in a sibling file so `crates/core/src/registry/mod.rs`
//! itself -- the facade -- never mentions a dummy type or a dummy id (a
//! grep-based test checks `mod.rs` specifically for that). The module path
//! stays `registry::tests::*`: `mod tests;` in `mod.rs` pulls this file's
//! content into that namespace exactly as an inline `mod tests { .. }`
//! block would.

use super::*;
use crate::params::ParamValues;

/// A synthetic stacking algorithm for feeding [`index`] and
/// [`index_or_panic`] without touching `inventory`'s real, process-wide
/// registry. Never `inventory::submit!`-ed.
struct SyntheticStack(&'static str);

impl StackAlgorithm for SyntheticStack {
    fn id(&self) -> &str {
        self.0
    }
    fn display_name(&self) -> &str {
        self.0
    }
    fn params(&self) -> &[ParamDesc] {
        &[]
    }
    fn fuse(
        &self,
        _ctx: &crate::algorithm::TileCtx<'_>,
        _cfg: &ParamValues,
    ) -> Result<crate::algorithm::TileOutput, crate::algorithm::AlgorithmError> {
        unreachable!("synthetic algorithm is never run, only indexed")
    }
}

/// Twin of [`SyntheticStack`] for the alignment family.
struct SyntheticAlign(&'static str);

impl AlignAlgorithm for SyntheticAlign {
    fn id(&self) -> &str {
        self.0
    }
    fn display_name(&self) -> &str {
        self.0
    }
    fn params(&self) -> &[ParamDesc] {
        &[]
    }
    fn align_frame(
        &self,
        _ctx: &crate::algorithm::AlignContext<'_>,
        _frame: usize,
        _cfg: &ParamValues,
    ) -> Result<crate::algorithm::AlignOutcome, crate::algorithm::AlgorithmError> {
        unreachable!("synthetic algorithm is never run, only indexed")
    }
}

static SYN_STACK_A: SyntheticStack = SyntheticStack("alpha");
static SYN_STACK_B: SyntheticStack = SyntheticStack("bravo");
static SYN_STACK_DUP: SyntheticStack = SyntheticStack("shared-id");
static SYN_ALIGN_DUP: SyntheticAlign = SyntheticAlign("shared-id");
static SYN_STACK_Z: SyntheticStack = SyntheticStack("zulu");
static SYN_STACK_M: SyntheticStack = SyntheticStack("mike");
static SYN_STACK_A2: SyntheticStack = SyntheticStack("alfa");

#[test]
fn duplicate_id_is_detected() {
    static SYN_STACK_DUP2: SyntheticStack = SyntheticStack("shared-id");
    let entries = [
        AlgorithmRef::Stack(&SYN_STACK_A),
        AlgorithmRef::Stack(&SYN_STACK_DUP),
        AlgorithmRef::Stack(&SYN_STACK_DUP2),
    ];
    let dup = index(entries).expect_err("two stacking algorithms shared an id");
    assert_eq!(dup.id, "shared-id");
    assert_eq!(dup.first, Family::Stack);
    assert_eq!(dup.second, Family::Stack);
    assert!(dup.to_string().contains("shared-id"));
}

#[test]
fn duplicate_id_is_detected_across_families() {
    let entries = [
        AlgorithmRef::Stack(&SYN_STACK_DUP),
        AlgorithmRef::Align(&SYN_ALIGN_DUP),
    ];
    let dup = index(entries).expect_err("a stack and an align algorithm shared an id");
    assert_eq!(dup.id, "shared-id");
    assert_eq!(dup.first, Family::Stack);
    assert_eq!(dup.second, Family::Align);
    assert!(dup.to_string().contains("shared-id"));
}

#[test]
#[should_panic(expected = "shared-id")]
fn a_duplicate_id_panics_naming_the_id() {
    let entries = [
        AlgorithmRef::Stack(&SYN_STACK_DUP),
        AlgorithmRef::Align(&SYN_ALIGN_DUP),
    ];
    index_or_panic(entries);
}

#[test]
fn listing_order_is_stable() {
    // Fed in descending id order; the index (a `BTreeMap`) must still
    // list them ascending -- by-id order is structural, not a
    // `sort_by_key` someone could delete.
    let entries = [
        AlgorithmRef::Stack(&SYN_STACK_Z),
        AlgorithmRef::Stack(&SYN_STACK_M),
        AlgorithmRef::Stack(&SYN_STACK_A2),
    ];
    let map = index(entries).expect("no duplicate ids among alfa/mike/zulu");
    let ids: Vec<&str> = map.keys().copied().collect();
    assert_eq!(ids, vec!["alfa", "mike", "zulu"]);
}

#[test]
fn distinct_synthetic_ids_index_cleanly() {
    let entries = [
        AlgorithmRef::Stack(&SYN_STACK_A),
        AlgorithmRef::Stack(&SYN_STACK_B),
    ];
    let map = index(entries).expect("alpha and bravo do not collide");
    assert_eq!(map.len(), 2);
}

#[cfg(feature = "test-algorithms")]
#[test]
fn resolves_a_stack_algorithm_by_id() {
    let algo = find_stack("dummy-stack").expect("dummy-stack is registered");
    assert_eq!(algo.id(), "dummy-stack");
}

#[cfg(feature = "test-algorithms")]
#[test]
fn resolves_an_align_algorithm_by_id() {
    let algo = find_align("dummy-align").expect("dummy-align is registered");
    assert_eq!(algo.id(), "dummy-align");
}

#[cfg(feature = "test-algorithms")]
#[test]
fn unknown_id_is_a_named_error() {
    let err = find("no-such-algorithm").expect_err("nothing registers this id");
    match &err {
        LookupError::Unknown { id, known } => {
            assert_eq!(id, "no-such-algorithm");
            assert!(known.contains(&"dummy-stack".to_string()));
            assert!(known.contains(&"dummy-align".to_string()));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
    let rendered = err.to_string();
    assert!(rendered.contains("no-such-algorithm"));
    assert!(rendered.contains("dummy-stack"));
    assert!(rendered.contains("dummy-align"));
}

#[cfg(feature = "test-algorithms")]
#[test]
fn an_id_from_the_other_family_is_not_unknown() {
    match find_stack("dummy-align") {
        Err(LookupError::WrongFamily {
            id,
            actual: Family::Align,
            wanted: Family::Stack,
        }) => assert_eq!(id, "dummy-align"),
        other => panic!("expected WrongFamily, got {:?}", other.map(|a| a.id())),
    }

    match find_align("dummy-stack") {
        Err(LookupError::WrongFamily {
            id,
            actual: Family::Stack,
            wanted: Family::Align,
        }) => assert_eq!(id, "dummy-stack"),
        other => panic!("expected WrongFamily, got {:?}", other.map(|a| a.id())),
    }
}

#[cfg(feature = "test-algorithms")]
#[test]
fn each_family_listing_holds_only_its_own_family() {
    let stacks = stack_algorithms();
    let aligns = align_algorithms();

    // Guards against the failure a combined-listing assertion cannot see: both of
    // these returning an empty Vec would leave every other test in this module green.
    let stack_ids: Vec<&str> = stacks.iter().map(|a| a.id()).collect();
    let align_ids: Vec<&str> = aligns.iter().map(|a| a.id()).collect();
    // `laplacian` joins the dummies here: it is the first real algorithm in
    // the registry, and it is a stack algorithm. `depth-map` and
    // `weighted-average` are two more stack algorithms, alphabetical
    // by id like every listing here. `pyramidal-ecc` is the align
    // listing's first real algorithm, so the align listing has grown too.
    // The assertion stays exact rather than becoming a `contains` -- its
    // job is to catch a registration silently disappearing, which a subset
    // check would not see.
    assert_eq!(
        stack_ids,
        vec!["depth-map", "dummy-stack", "laplacian", "weighted-average"]
    );
    assert_eq!(align_ids, vec!["dummy-align", "pyramidal-ecc"]);

    assert_eq!(
        stacks.len() + aligns.len(),
        algorithms().len(),
        "the two family listings must partition the combined listing"
    );
    for algo in &stacks {
        assert!(!algo.display_name().is_empty());
        assert!(!algo.params().is_empty());
    }
    for algo in &aligns {
        assert!(!algo.display_name().is_empty());
        assert!(!algo.params().is_empty());
    }
}

#[cfg(feature = "test-algorithms")]
#[test]
fn lists_every_registered_algorithm() {
    let listing = algorithms();
    let ids: Vec<&str> = listing.iter().map(AlgorithmRef::id).collect();
    assert_eq!(
        ids,
        vec![
            "depth-map",
            "dummy-align",
            "dummy-stack",
            "laplacian",
            "pyramidal-ecc",
            "weighted-average"
        ],
        "the listing must contain exactly the two dummies plus the four registered \
         real algorithms, in this order"
    );
    for entry in &listing {
        assert!(!entry.display_name().is_empty());
        assert!(
            !entry.params().is_empty(),
            "{} must have a non-empty schema",
            entry.id()
        );
    }
}

#[cfg(feature = "test-algorithms")]
#[test]
fn schema_is_reachable_from_the_listing() {
    let entry = find("dummy-align").expect("dummy-align is registered");
    let schema: &'static [ParamDesc] = entry.params();
    assert!(!schema.is_empty());

    let mut values = ParamValues::new();
    values.insert("max-shift".to_string(), crate::params::ParamValue::Int(10));
    crate::params::validate(schema, &values)
        .expect("a legal value validates against the listed schema");
}
