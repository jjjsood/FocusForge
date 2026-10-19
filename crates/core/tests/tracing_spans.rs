// SPDX-License-Identifier: GPL-3.0-or-later

//! Span context around the work this crate fans out: the tile scheduler's
//! rayon pool and [`focusforge_core::timing::StageTimer`]'s stage
//! boundaries. A span entered on the calling thread is not current inside a
//! pool closure by itself, so these tests assert the chain an event
//! actually carries rather than the chain the call site looks like it has.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus,
};
use focusforge_core::align::{AlignDriverOptions, FailurePolicy, Transform, align_stack_with};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{ParamDesc, ParamValues, StackContext};
use focusforge_core::tile::{
    NoopPreprocess, TileConsumer, TileJob, TileScheduler, TileSchedulerOptions,
};
use focusforge_core::timing::{Stage, StageTimer, measurement_scope};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

/// One span chain, outermost first, each entry `name` or
/// `name{field=value,...}`.
type Chain = Vec<String>;

/// What the capturing layer collected: the chain of every event, and the
/// chain of every span at the moment it was created.
#[derive(Clone, Default)]
struct Captured {
    events: Arc<Mutex<Vec<Chain>>>,
    spans: Arc<Mutex<Vec<Chain>>>,
}

impl Captured {
    /// Every event chain rooted in the span whose `root` field is `root`.
    fn events_under(&self, root: &str) -> Vec<Chain> {
        Self::under(&self.events, root)
    }

    /// Every span chain rooted in that same span, the span's own name last.
    fn spans_under(&self, root: &str) -> Vec<Chain> {
        Self::under(&self.spans, root)
    }

    fn under(buffer: &Mutex<Vec<Chain>>, root: &str) -> Vec<Chain> {
        buffer
            .lock()
            .unwrap()
            .iter()
            .filter(|chain| chain.first().is_some_and(|first| first.contains(root)))
            .cloned()
            .collect()
    }
}

/// A span's fields, rendered once at creation and kept in the registry's
/// per-span extensions.
struct RenderedFields(String);

#[derive(Default)]
struct FieldRenderer(String);

impl Visit for FieldRenderer {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(',');
        }
        self.0.push_str(&format!("{}={value:?}", field.name()));
    }
}

struct CaptureLayer(Captured);

impl<S> Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let mut fields = FieldRenderer::default();
        attrs.record(&mut fields);
        let span = ctx.span(id).expect("a new span is always in the registry");
        span.extensions_mut().insert(RenderedFields(fields.0));
        let chain = span.scope().from_root().map(render).collect();
        self.0.spans.lock().unwrap().push(chain);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let chain = ctx
            .event_scope(event)
            .map(|scope| scope.from_root().map(render).collect())
            .unwrap_or_default();
        self.0.events.lock().unwrap().push(chain);
    }
}

fn render<S>(span: tracing_subscriber::registry::SpanRef<'_, S>) -> String
where
    S: for<'a> LookupSpan<'a>,
{
    let extensions = span.extensions();
    let fields = extensions
        .get::<RenderedFields>()
        .map(|f| f.0.as_str())
        .unwrap_or_default();
    if fields.is_empty() {
        span.name().to_string()
    } else {
        format!("{}{{{fields}}}", span.name())
    }
}

/// The process-wide capturing subscriber, installed on first use. A pool
/// thread never sees a thread-local subscriber, so this is global; every
/// test scopes its own run under a uniquely named root span and reads back
/// only the chains under that name.
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

fn has(chain: &Chain, name: &str) -> bool {
    chain.iter().any(|entry| entry.starts_with(name))
}

/// Raises one event per tile from the pool thread that runs it, and reads
/// no pixels: what is under test is the chain that event carries.
struct EventRaisingConsumer;

impl TileConsumer for EventRaisingConsumer {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        tracing::debug!(interior = ?job.interior(), "consumed");
        Ok(())
    }
}

fn scheduler_options(label: &str, max_bytes_in_flight: u64) -> TileSchedulerOptions {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("tracing_spans_{label}"));
    let mut options = TileSchedulerOptions::explicit_budget(base, max_bytes_in_flight);
    options.tile_size = 200;
    options.levels = 3;
    options
}

/// Runs the scheduler over `fixtures/mantis-512` under a `fuse` stage span
/// inside a root span named `root`, and returns the planned tile count.
fn run_under_root(root: &'static str, label: &str, max_bytes_in_flight: u64) -> usize {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);
    let options = scheduler_options(label, max_bytes_in_flight);
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, 3, &options).expect("scheduler must build");
    let planned = scheduler.plan().len();

    let root_span = tracing::debug_span!("root", root);
    let _root = root_span.enter();
    let mut timer = StageTimer::new();
    timer.enter(Stage::Fuse);
    scheduler
        .run(&NoopPreprocess, &EventRaisingConsumer)
        .expect("run must succeed");
    timer.mark(Stage::Fuse);
    planned
}

/// One tile in flight and eight: the same `max_bytes_in_flight` pair
/// the scheduler's own budget tests use.
fn budgets() -> (u64, u64) {
    let bytes_per_tile = 200u64 * 200 * (3 * 2 + 1);
    (bytes_per_tile, bytes_per_tile * 8)
}

#[test]
fn tile_span_and_stage_span_reach_an_event_raised_on_a_pool_thread() {
    let captured = captured();
    let (one_set, eight_sets) = budgets();

    for (root, label, budget) in [
        ("tile_span_one_set", "tile_span_one", one_set),
        ("tile_span_eight_sets", "tile_span_eight", eight_sets),
    ] {
        run_under_root(root, label, budget);

        let events = captured.events_under(root);
        assert!(
            !events.is_empty(),
            "no event was raised under {root} at all"
        );
        for chain in &events {
            assert!(
                has(chain, "stage"),
                "an event from a pool thread lost its stage span: {chain:?}"
            );
            assert!(
                has(chain, "tile"),
                "an event from a pool thread lost its tile span: {chain:?}"
            );
            assert!(
                chain.iter().any(|entry| entry.contains("fuse")),
                "the stage span on a pool thread is not the one the run entered: {chain:?}"
            );
        }
    }
}

#[test]
fn span_count_follows_the_planned_tile_count_at_either_budget() {
    let captured = captured();
    let (one_set, eight_sets) = budgets();

    for (root, label, budget) in [
        ("span_count_one_set", "span_count_one", one_set),
        ("span_count_eight_sets", "span_count_eight", eight_sets),
    ] {
        let planned = run_under_root(root, label, budget);
        let tile_spans = captured
            .spans_under(root)
            .into_iter()
            .filter(|chain| chain.last().is_some_and(|last| last.starts_with("tile")))
            .count();
        assert_eq!(
            tile_spans, planned,
            "{root} opened {tile_spans} tile spans for {planned} planned tiles"
        );
    }
}

#[test]
fn a_stage_span_covers_exactly_its_own_interval() {
    let captured = captured();
    let root = "stage_span";
    let root_span = tracing::debug_span!("root", root);
    let _root = root_span.enter();

    let mut timer = StageTimer::new();
    timer.enter(Stage::Setup);
    tracing::debug!("in setup");
    timer.mark(Stage::Setup);
    timer.enter(Stage::Align);
    tracing::debug!("in align");
    timer.mark(Stage::Align);
    tracing::debug!("between stages");

    let events = captured.events_under(root);
    assert_eq!(events.len(), 3, "expected three events, got {events:?}");
    assert!(
        events[0].iter().any(|e| e.contains("setup"))
            && !events[0].iter().any(|e| e.contains("align")),
        "the first event must carry the setup stage alone: {:?}",
        events[0]
    );
    assert!(
        events[1].iter().any(|e| e.contains("align"))
            && !events[1].iter().any(|e| e.contains("setup")),
        "the second event must carry the align stage alone: {:?}",
        events[1]
    );
    assert!(
        !has(&events[2], "stage"),
        "a stage span outlived its mark: {:?}",
        events[2]
    );
}

#[test]
fn timing_scopes_are_absent_before_diagnostics_enables_them() {
    let captured = captured();
    let root = "timing_disabled";
    let root_span = tracing::debug_span!("root", root);
    let _root = root_span.enter();
    let scope = measurement_scope("tiff_read");
    tracing::debug!("ordinary event");
    drop(scope);

    assert!(
        captured
            .spans_under(root)
            .into_iter()
            .all(|chain| !has(&chain, "timing")),
        "timing scopes must not create trace records before diagnostics enables them"
    );
}

/// Raises one event per frame from the pool thread that registers it and
/// solves nothing: what is under test is the chain that event carries.
struct EventRaisingAlign;

impl AlignAlgorithm for EventRaisingAlign {
    fn id(&self) -> &str {
        "event-raising"
    }

    fn display_name(&self) -> &str {
        "Event raising"
    }

    fn params(&self) -> &[ParamDesc] {
        &[]
    }

    fn align_frame(
        &self,
        _ctx: &AlignContext<'_>,
        frame: usize,
        _cfg: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        tracing::debug!(frame, "registered");
        Ok(AlignOutcome {
            transform: Transform::identity(),
            status: AlignStatus::Converged,
            iterations: 0,
            residual: 1.0,
        })
    }
}

#[test]
fn frame_span_reaches_an_event_raised_on_an_alignment_pool_thread() {
    let captured = captured();
    let root = "frame_span";
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(stack.len(), width, height, 3);

    let root_span = tracing::debug_span!("root", root);
    let _root = root_span.enter();
    let mut timer = StageTimer::new();
    timer.enter(Stage::Align);
    let run = align_stack_with(
        &EventRaisingAlign,
        stack_ctx,
        &stack,
        &ParamValues::new(),
        FailurePolicy::Strict,
        &AlignDriverOptions::default(),
    )
    .expect("alignment must succeed");
    timer.mark(Stage::Align);
    assert!(
        run.worker_limit > 1,
        "this host ran the serial arm, so no pool thread was exercised"
    );

    let events = captured.events_under(root);
    assert!(
        !events.is_empty(),
        "no event was raised under {root} at all"
    );
    for chain in &events {
        assert!(
            has(chain, "frame"),
            "an event from an alignment pool thread lost its frame span: {chain:?}"
        );
        assert!(
            chain.iter().any(|entry| entry.contains("align")),
            "an event from an alignment pool thread lost its stage span: {chain:?}"
        );
    }
}
