// SPDX-License-Identifier: GPL-3.0-or-later
//! The graphical front end's spans draw on their own admission budget, so a
//! long processing run that exhausts the processing budget never refuses them.
//! One test function: accounting is a process-wide switch.

use focusforge_core::timing::{
    TimingCounter, enable_trace_accounting, measurement_scope, measurement_scope_with_fields,
    record_timing, timing_enabled, timing_snapshot,
};

const PROCESSING_BUDGET: u64 = 262_144;
const OVERFLOW: u64 = 37;

#[test]
fn gui_spans_are_admitted_after_the_processing_budget_refuses() {
    drop(measurement_scope("gpu_cpu_submit"));
    drop(measurement_scope_with_fields(
        "gui_input",
        0,
        0,
        1,
        0,
        false,
        0,
    ));
    record_timing(TimingCounter::CopyBytes, 1);
    assert!(!timing_enabled());
    assert!(timing_snapshot().is_none());

    enable_trace_accounting();
    for _ in 0..PROCESSING_BUDGET + OVERFLOW {
        drop(measurement_scope("gpu_cpu_submit"));
    }
    drop(measurement_scope("gui_frame_rendered"));
    drop(measurement_scope_with_fields(
        "gui_input",
        0,
        0,
        1,
        0,
        false,
        0,
    ));

    let snapshot = timing_snapshot().expect("accounting is enabled");
    assert_eq!(snapshot.admitted, PROCESSING_BUDGET);
    assert_eq!(snapshot.exhausted, OVERFLOW);
    assert_eq!(snapshot.gui_admitted, 2);
    assert_eq!(snapshot.gui_exhausted, 0);
}
