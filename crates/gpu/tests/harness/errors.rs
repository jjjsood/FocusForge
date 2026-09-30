// SPDX-License-Identifier: GPL-3.0-or-later

//! The shared device's error handlers, observed through a capturing
//! `tracing` layer installed in this test binary: a validation error raised
//! inside a scope names the kernel that was dispatched, and one raised
//! outside every scope is logged rather than routed into wgpu's own
//! handler, which panics.
//!
//! Both tests raise their error by asking for a buffer larger than the
//! device's `max_buffer_size` -- a validation failure wgpu rejects before
//! it allocates anything, so neither test costs device memory.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use focusforge_core::io::TiffStack;
use focusforge_core::tile::Rect;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

use crate::common;

#[derive(Clone, Debug)]
struct Record {
    level: tracing::Level,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<Record>>>);

impl Captured {
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn records(&self) -> Vec<Record> {
        self.0.lock().unwrap().clone()
    }
}

struct CaptureLayer(Captured);

#[derive(Default)]
struct FieldVisitor(BTreeMap<String, String>);

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.0.0.lock().unwrap().push(Record {
            level: *event.metadata().level(),
            fields: visitor.0,
        });
    }
}

/// The process-wide capturing subscriber, installed on first use: a
/// subscriber can be made global only once, and both tests below need one.
fn captured() -> Captured {
    static CAPTURED: OnceLock<Captured> = OnceLock::new();
    CAPTURED
        .get_or_init(|| {
            let captured = Captured::default();
            tracing_subscriber::registry()
                .with(CaptureLayer(captured.clone()))
                .init();
            captured
        })
        .clone()
}

/// Both tests below clear the shared capture buffer and then read it back,
/// so they must not overlap.
fn exclusive() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A buffer descriptor wgpu refuses: `max_buffer_size` is the largest the
/// device validates, so one word past it is a validation error.
fn oversized_buffer(device: &wgpu::Device) -> wgpu::BufferDescriptor<'static> {
    wgpu::BufferDescriptor {
        label: Some("focusforge-gpu-oversized-probe"),
        size: device.limits().max_buffer_size + 4,
        usage: wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    }
}

fn error_records(captured: &Captured) -> Vec<Record> {
    captured
        .records()
        .into_iter()
        .filter(|record| record.level == tracing::Level::ERROR)
        .collect()
}

#[test]
fn validation_error_names_the_kernel() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let Ok(backend) = focusforge_gpu::harness::gpu_backend() else {
        println!("SKIP: no usable GPU backend");
        return;
    };
    let captured = captured();
    let _serial = exclusive();

    // A real dispatch first, on this very thread: error scopes and the
    // dispatch label are both thread-local, so the label the record names
    // has to be recorded here and not on a pool thread.
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let spp = stack.header(0).samples_per_pixel;
    let rect = Rect {
        x: 0,
        y: 0,
        width: 64,
        height: 64,
    };
    focusforge_core::gpu::TileBackend::self_check(backend, &stack, 0, rect, spp, 1.0)
        .expect("the self-check kernel must run");

    let device = shared.device();
    captured.clear();
    let outcome = focusforge_gpu::errors::with_validation_scope(device, || {
        let _buffer = device.create_buffer(&oversized_buffer(device));
    });
    match outcome {
        Err(focusforge_gpu::device::GpuError::KernelValidation { kernel, detail }) => {
            assert_eq!(kernel, "focusforge-gpu-harness-pass");
            assert!(detail.contains("maximum buffer size"), "detail: {detail}");
        }
        other => panic!("a validation error must fail the scope, got {other:?}"),
    }

    let records = error_records(&captured);
    let named = records
        .iter()
        .find(|record| record.fields.contains_key("kernel"))
        .unwrap_or_else(|| panic!("expected an ERROR naming a kernel, captured: {records:?}"));
    assert_eq!(
        named.fields.get("kernel").map(String::as_str),
        Some("focusforge-gpu-harness-pass"),
        "the record must name the kernel dispatched on this thread: {named:?}"
    );
    assert!(
        named
            .fields
            .get("detail")
            .is_some_and(|detail| detail.contains("maximum buffer size")),
        "the record must carry wgpu's own message: {named:?}"
    );
}

#[test]
fn uncaptured_error_is_logged() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let captured = captured();
    let _serial = exclusive();

    let device = shared.device();
    captured.clear();
    // No scope of any kind is open here: without an installed handler wgpu
    // would take this through `default_error_handler`, which panics, so
    // reaching the assertions below is half of what this test proves.
    let _buffer = device.create_buffer(&oversized_buffer(device));

    let records = error_records(&captured);
    assert!(
        records
            .iter()
            .any(|record| record.fields.get("kind").map(String::as_str) == Some("validation")),
        "expected an ERROR naming the error's kind, captured: {records:?}"
    );
}
