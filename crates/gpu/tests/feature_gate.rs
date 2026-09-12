// SPDX-License-Identifier: GPL-3.0-or-later

#![cfg(feature = "gpu")]

#[test]
#[allow(clippy::assertions_on_constants)]
fn gpu_feature_gate_is_wired() {
    assert!(cfg!(feature = "gpu"));
}
