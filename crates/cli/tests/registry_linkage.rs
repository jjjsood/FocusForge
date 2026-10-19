// SPDX-License-Identifier: GPL-3.0-or-later

//! Proves the linker caveat documented on `focusforge_core::registry`: a
//! registration made in `crates/core/src/test_algorithms/*.rs` reaches a
//! separate crate that links `focusforge-core`'s rlib and imports **only**
//! `focusforge_core::registry` -- never `focusforge_core::test_algorithms`
//! or either dummy type by name. If a registration in a module nothing
//! references were dropped by the linker, both tests below would fail.
//!
//! The feature that makes the dummies exist in this build is supplied by
//! `crates/cli/Cargo.toml`'s `[dev-dependencies]` entry on `focusforge-core`,
//! not by any flag on this test binary -- that being sufficient is itself
//! part of what this test demonstrates.

use focusforge_core::registry;

#[test]
fn core_registrations_reach_a_downstream_crate() {
    let listing = registry::algorithms();
    let ids: Vec<&str> = listing.iter().map(registry::AlgorithmRef::id).collect();
    assert!(
        ids.contains(&"dummy-stack"),
        "dummy-stack must reach this crate through registry::algorithms(), got {ids:?}"
    );
    assert!(
        ids.contains(&"dummy-align"),
        "dummy-align must reach this crate through registry::algorithms(), got {ids:?}"
    );

    for entry in &listing {
        assert!(
            !entry.display_name().is_empty(),
            "{} must carry a display name",
            entry.id()
        );
        assert!(
            !entry.params().is_empty(),
            "{} must carry a non-empty schema",
            entry.id()
        );
    }
}

#[test]
fn a_downstream_crate_resolves_by_id() {
    let stack = registry::find_stack("dummy-stack").expect("dummy-stack resolves by id");
    assert_eq!(stack.id(), "dummy-stack");

    let align = registry::find_align("dummy-align").expect("dummy-align resolves by id");
    assert_eq!(align.id(), "dummy-align");
}

/// `pyramidal-ecc` is a production registration (no `test-algorithms`
/// feature involved), so this proves the same linker caveat against a real
/// algorithm, not only the test-only dummies.
#[test]
fn pyramidal_ecc_reaches_a_downstream_crate() {
    let listing = registry::algorithms();
    let ids: Vec<&str> = listing.iter().map(registry::AlgorithmRef::id).collect();
    assert!(
        ids.contains(&"pyramidal-ecc"),
        "pyramidal-ecc must reach this crate through registry::algorithms(), got {ids:?}"
    );

    let align = registry::find_align("pyramidal-ecc").expect("pyramidal-ecc resolves by id");
    assert_eq!(align.id(), "pyramidal-ecc");
    assert_eq!(align.display_name(), "Pyramidal ECC");
    assert_eq!(align.params().len(), 6);
}
