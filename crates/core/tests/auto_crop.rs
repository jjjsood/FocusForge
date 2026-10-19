// SPDX-License-Identifier: GPL-3.0-or-later

//! Auto-crop derived from a real alignment of `fixtures/mantis-512` -- the
//! only test here that touches actual measured drift; every other case
//! lives in `align::crop`'s own unit tests.

mod common;

use focusforge_core::algorithm::AlignAlgorithm;
use focusforge_core::align::{
    FailurePolicy, KERNEL_RADIUS, PyramidalEcc, Transform, align_stack, auto_crop,
};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{self, ParamValues, StackContext};
use focusforge_core::tile::Rect;

fn open_fixture() -> TiffStack {
    TiffStack::open(common::mantis_512_dir()).expect("opening fixtures/mantis-512")
}

/// Fully resolved defaults for `PyramidalEcc` against `ctx`.
fn resolved_defaults(ctx: &StackContext) -> ParamValues {
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), ctx)
        .expect("defaults resolve cleanly");
    params::resolved_values(&resolved)
}

/// Every corner of `rect`, mapped forward through every one of
/// `transforms`, lands inside that frame's kernel-inset interior -- the
/// containment invariant `align::crop`'s own unit tests check, applied here
/// to a real (not synthetic) alignment run. Independent of `auto_crop`
/// itself: it re-derives containment from `Transform::map_point` directly,
/// never from the crop module's own polygon arithmetic.
fn assert_contained(rect: Rect, transforms: &[Transform], frame: (u32, u32)) {
    let r = KERNEL_RADIUS as f64;
    let (w, h) = (frame.0 as f64, frame.1 as f64);
    let corners = [
        (rect.x as f64, rect.y as f64),
        ((rect.x + rect.width - 1) as f64, rect.y as f64),
        (rect.x as f64, (rect.y + rect.height - 1) as f64),
        (
            (rect.x + rect.width - 1) as f64,
            (rect.y + rect.height - 1) as f64,
        ),
    ];
    for t in transforms {
        for &(x, y) in &corners {
            let (fx, fy) = t
                .map_point(x, y)
                .expect("a crop corner stays inside every footprint's domain");
            assert!(
                fx >= r - 1e-6 && fx <= w - 1.0 - r + 1e-6,
                "fx {fx} outside [{r}, {}] for {t:?}",
                w - 1.0 - r
            );
            assert!(
                fy >= r - 1e-6 && fy <= h - 1.0 - r + 1e-6,
                "fy {fy} outside [{r}, {}] for {t:?}",
                h - 1.0 - r
            );
        }
    }
}

/// Measured 2026-09-13 (`cargo test -p focusforge-core --test auto_crop
/// crop_of_the_real_fixture_drift -- --nocapture`): against the eight
/// `fixtures/mantis-512` frames under `FailurePolicy::IdentityFallback`,
/// middle-anchor reference (frame 3): crop = `Rect { x: 31, y: 40, width:
/// 422, height: 424 }`, inset full frame = `(508, 508)`. Three frames fall
/// back to identity (0, 1 and 7 -- each `Diverged`, `fallback=true`) and
/// contribute the inset rect unshrunk; frames 2, 4, 5 and 6 report a
/// similarity with a real rotation/scale component (off-diagonal terms up
/// to `0.013`, scale up to `1.108`), and their combined drift shrinks both
/// axes -- width from 508 to 422, height from 508 to 424 -- because a
/// rotated footprint's inset rectangle shrinks in both directions, unlike a
/// pure translation.
#[test]
fn crop_of_the_real_fixture_drift() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let cfg = resolved_defaults(&stack_ctx);

    let alignment = align_stack(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::IdentityFallback,
    )
    .expect("real fixture frames register");

    let rect = auto_crop(&alignment, (width, height))
        .expect("every transform this driver produces is affine")
        .expect("some overlap survives on this fixture");

    let r = KERNEL_RADIUS;
    let inset_full = (width - 2 * r, height - 2 * r);
    eprintln!("crop_of_the_real_fixture_drift: crop = {rect:?}, inset full frame = {inset_full:?}");
    for f in &alignment.frames {
        eprintln!(
            "  frame {} status={:?} fallback={} transform={:?}",
            f.frame, f.status, f.fallback, f.transform
        );
    }

    assert!(
        rect.width < inset_full.0 || rect.height < inset_full.1,
        "crop {rect:?} is not smaller than the inset full frame {inset_full:?} on any axis -- \
         real drift did not shrink it"
    );

    let transforms: Vec<Transform> = alignment.frames.iter().map(|f| f.transform).collect();
    assert_contained(rect, &transforms, (width, height));
}

/// Measured 2026-09-13 (`-- --nocapture`): under `FailurePolicy::Strict`
/// every diverged frame keeps its solver-reported best-rho transform
/// instead of falling back to identity -- frame 1's is a real similarity
/// (`scale 1.09`, rotation ~3.2 deg) and frame 7's is a larger one (`scale
/// 1.23`, rotation ~0.564 deg), both angles computed as
/// `asin(off_diagonal / scale)`, both from a divergence *after* real motion
/// was found, unlike frame 0's, which is still identity (diverged on the very
/// first Newton step). The crop shrinks further than the identity-fallback
/// case as a result: `Rect { x: 40, y: 97, width: 339, height: 355 }`. Only
/// containment is asserted: a diverged-with-motion frame's best-rho
/// transform has no bound that can be predicted in general.
#[test]
fn crop_under_strict_failure_policy_stays_inside_every_footprint() {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let cfg = resolved_defaults(&stack_ctx);

    let alignment = align_stack(
        &PyramidalEcc,
        stack_ctx,
        &stack,
        &cfg,
        FailurePolicy::Strict,
    )
    .expect("real fixture frames register");

    let rect = auto_crop(&alignment, (width, height))
        .expect("every transform this driver produces is affine");
    eprintln!("crop_under_strict_failure_policy_stays_inside_every_footprint: crop = {rect:?}");
    for f in &alignment.frames {
        eprintln!(
            "  frame {} status={:?} fallback={} transform={:?}",
            f.frame, f.status, f.fallback, f.transform
        );
    }

    if let Some(rect) = rect {
        let transforms: Vec<Transform> = alignment.frames.iter().map(|f| f.transform).collect();
        assert_contained(rect, &transforms, (width, height));
    }
}
