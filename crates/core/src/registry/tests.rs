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

/// An alignment entry carrying the two default-rule markers, for feeding
/// [`default_in`].
struct MarkedAlign {
    id: &'static str,
    test_only: bool,
    default: bool,
}

impl AlignAlgorithm for MarkedAlign {
    fn id(&self) -> &str {
        self.id
    }
    fn display_name(&self) -> &str {
        self.id
    }
    fn params(&self) -> &[ParamDesc] {
        &[]
    }
    fn test_only(&self) -> bool {
        self.test_only
    }
    fn is_default(&self) -> bool {
        self.default
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

const fn marked(id: &'static str, test_only: bool, default: bool) -> MarkedAlign {
    MarkedAlign {
        id,
        test_only,
        default,
    }
}

static REAL_ZULU: MarkedAlign = marked("zulu", false, false);
static REAL_MIKE: MarkedAlign = marked("mike", false, false);
static REAL_MIKE_DEFAULT: MarkedAlign = marked("mike", false, true);
static REAL_ZULU_DEFAULT: MarkedAlign = marked("zulu", false, true);
static TEST_ONLY_ALFA: MarkedAlign = marked("alfa", true, false);
static TEST_ONLY_BRAVO: MarkedAlign = marked("bravo", true, true);

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

// The default rule's inputs below are fed in descending id order, so a
// `.first()` over them, or over the id-sorted listing, cannot pass.

#[test]
fn the_sole_real_align_entry_is_the_default_over_a_test_only_one() {
    let entries = [
        AlgorithmRef::Align(&REAL_ZULU),
        AlgorithmRef::Align(&TEST_ONLY_ALFA),
    ];
    let default = default_in(entries, Family::Align).expect("one real entry is the default");
    assert_eq!(default.id(), "zulu");
}

#[test]
fn two_real_align_entries_are_refused_naming_both() {
    let entries = [
        AlgorithmRef::Align(&REAL_ZULU),
        AlgorithmRef::Align(&REAL_MIKE),
        AlgorithmRef::Align(&TEST_ONLY_ALFA),
    ];
    let err = default_in(entries, Family::Align).expect_err("two real entries are ambiguous");
    let message = err.to_string();
    assert!(
        message.starts_with("multiple alignment algorithms are registered"),
        "{message}"
    );
    assert!(
        message.contains("zulu") && message.contains("mike"),
        "{message}"
    );
    assert!(
        !message.contains("alfa"),
        "a test-only entry is never a candidate: {message}"
    );
}

#[test]
fn only_test_only_align_entries_leave_no_default() {
    // A test-only entry marked default still never counts.
    let entries = [
        AlgorithmRef::Align(&TEST_ONLY_BRAVO),
        AlgorithmRef::Align(&TEST_ONLY_ALFA),
    ];
    let err = default_in(entries, Family::Align).expect_err("nothing real is registered");
    assert_eq!(err.to_string(), "no alignment algorithm is registered");
}

#[test]
fn a_stacking_entry_is_never_the_align_default() {
    let entries = [AlgorithmRef::Stack(&SYN_STACK_A)];
    let err = default_in(entries, Family::Align).expect_err("no align entry is registered");
    assert_eq!(err.to_string(), "no alignment algorithm is registered");
}

#[test]
fn the_marked_real_entry_wins_over_its_unmarked_siblings() {
    let entries = [
        AlgorithmRef::Align(&REAL_ZULU),
        AlgorithmRef::Align(&REAL_MIKE_DEFAULT),
        AlgorithmRef::Align(&TEST_ONLY_BRAVO),
    ];
    let default = default_in(entries, Family::Align).expect("one real entry is marked");
    assert_eq!(default.id(), "mike");
}

#[test]
fn two_marked_real_entries_are_refused_naming_them() {
    let entries = [
        AlgorithmRef::Align(&REAL_ZULU_DEFAULT),
        AlgorithmRef::Align(&REAL_MIKE_DEFAULT),
    ];
    let err = default_in(entries, Family::Align).expect_err("two marked entries are ambiguous");
    let message = err.to_string();
    assert!(
        message.contains("zulu") && message.contains("mike"),
        "{message}"
    );
}

#[cfg(feature = "test-algorithms")]
#[test]
fn the_real_registry_defaults_skip_the_test_only_entries() {
    assert_eq!(
        default_align().expect("one real align").id(),
        "pyramidal-ecc"
    );
    assert_eq!(
        default_stack().expect("laplacian is marked").id(),
        "laplacian"
    );
    for entry in algorithms() {
        let expected = matches!(entry.id(), "dummy-align" | "dummy-stack");
        assert_eq!(entry.test_only(), expected, "{}", entry.id());
    }
}

/// The ids of `params` a front end shows in its first view.
fn outside_advanced(params: &[ParamDesc]) -> Vec<&str> {
    params
        .iter()
        .filter(|p| !p.advanced)
        .map(|p| p.id.as_str())
        .collect()
}

/// `Err` naming the ids when a schema shows more than one parameter in the
/// first view.
fn at_most_one_outside_advanced(params: &[ParamDesc]) -> Result<(), String> {
    match outside_advanced(params).as_slice() {
        [] | [_] => Ok(()),
        many => Err(format!("more than one first-view parameter: {many:?}")),
    }
}

#[test]
fn every_stack_algorithm_shows_at_most_one_parameter_outside_advanced() {
    let real: Vec<&dyn StackAlgorithm> = stack_algorithms()
        .into_iter()
        .filter(|a| !a.test_only())
        .collect();
    assert_eq!(
        real.iter().map(|a| a.id()).collect::<Vec<_>>(),
        vec!["depth-map", "laplacian", "weighted-average"]
    );
    for entry in &real {
        at_most_one_outside_advanced(entry.params())
            .unwrap_or_else(|why| panic!("{}: {why}", entry.id()));
    }
    let shown = |id: &str| outside_advanced(find(id).expect("registered").params());
    assert!(shown("laplacian").is_empty());
    assert_eq!(shown("weighted-average"), vec!["weight-exponent"]);
    assert!(find("depth-map").expect("registered").advanced());
    assert!(
        !default_stack().expect("a default is marked").advanced(),
        "the default is never advanced"
    );
}

#[test]
fn a_schema_with_two_non_advanced_params_fails_the_walk() {
    let crop = crate::project::PROJECT_PARAMS
        .iter()
        .find(|d| d.id == "auto-crop")
        .expect("auto-crop is a project parameter")
        .clone();
    let mut second = crop.clone();
    second.id = "second".to_string();
    let schema = vec![crop, second];
    let why = at_most_one_outside_advanced(&schema).expect_err("two shown parameters");
    assert!(why.contains("auto-crop") && why.contains("second"), "{why}");
}

#[test]
fn every_alignment_parameter_is_advanced() {
    for entry in align_algorithms().iter().filter(|a| !a.test_only()) {
        for p in entry.params() {
            assert!(p.advanced, "{}: {} is shown up front", entry.id(), p.id);
        }
    }
    assert_eq!(
        outside_advanced(&crate::project::PROJECT_PARAMS),
        vec!["align", "auto-crop"]
    );
}

#[test]
fn role_first_display_names() {
    let name = |id: &str| find(id).expect("registered").display_name().to_string();
    assert_eq!(name("laplacian"), "Details");
    assert_eq!(name("weighted-average"), "Soft transitions");
    assert_eq!(name("depth-map"), "Focus map");
    let exponent = find("weighted-average")
        .expect("registered")
        .params()
        .iter()
        .find(|p| p.id == "weight-exponent")
        .expect("weight-exponent is declared")
        .label
        .clone();
    assert_eq!(exponent, "Sharp-source preference");
}

#[test]
fn fusion_and_crop_help_is_one_plain_sentence() {
    let mut described: Vec<(String, &ParamDesc)> = crate::project::PROJECT_PARAMS
        .iter()
        .map(|p| ("project".to_string(), p))
        .collect();
    for entry in stack_algorithms().iter().filter(|a| !a.test_only()) {
        described.extend(entry.params().iter().map(|p| (entry.id().to_string(), p)));
    }
    assert_eq!(
        described.len(),
        5 + 1 + 2 + 2,
        "the five project parameters plus the three schemas"
    );
    for (owner, p) in described {
        let help = p.help.as_str();
        let at = format!("{owner}/{}: {help:?}", p.id);
        assert!(!help.trim().is_empty(), "{at}");
        assert!(!help.contains('`') && !help.contains('\n'), "{at}");
        assert!(help.ends_with('.'), "{at}");
        assert!(
            !help.trim_end_matches('.').contains(". "),
            "more than one sentence: {at}"
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
