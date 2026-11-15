// SPDX-License-Identifier: GPL-3.0-or-later

//! What the live stage view costs a fuse run: the same run with the pane's
//! subscription against without it, plus each view's own in-run time from the
//! preview feed. The budget per view is 1 % of the unsubscribed wall time and
//! 5 s, whichever is smaller. The `#[ignore]`d legs read the reference stack
//! (`FOCUSFORGE_REFERENCE_STACK`) and take minutes; the fixture runs check the
//! measurement code itself.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use focusforge_core::display::PreviewCost;
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::job::JobControl;
use focusforge_core::params::ParamValue;
use focusforge_core::pipeline::{FuseOutcome, FuseProduct};
use focusforge_gui::job::{self, GuiObserver, HeatRamp, JobRequest, PreviewKinds};

/// The frames of a long run: the reference frames repeated up to this many.
const LONG_RUN_FRAMES: usize = 600;

/// The most one view may cost a run, in seconds.
const BUDGET_SECONDS: f64 = 5.0;

/// The most one view may cost a run, as a fraction of its wall time.
const BUDGET_FRACTION: f64 = 0.01;

const ALL_VIEWS: PreviewKinds = PreviewKinds {
    aligned_frame: true,
    composite: true,
    heat: true,
};

struct Run {
    wall: f64,
    cost: PreviewCost,
    megapixels: f64,
    frames: usize,
    composite: PathBuf,
    dirs: [PathBuf; 2],
}

/// One fuse run of `template` through the worker the window uses, subscribed
/// to `kinds` when given. Every run gets its own scratch root so none reuses
/// another's alignment.
fn run_once(
    template: &JobRequest,
    tag: &str,
    kinds: Option<PreviewKinds>,
    vary: Option<&dyn Fn(&mut JobRequest)>,
) -> Run {
    let mut request = template.clone();
    request.hold_dir = common::tree(&format!("svb_{tag}_hold"));
    request.scratch_root = common::tree(&format!("svb_{tag}_scratch"));
    request.staging_key = format!("svb-{tag}");
    if let Some(vary) = vary {
        vary(&mut request);
    }
    let dirs = [request.hold_dir.clone(), request.scratch_root.clone()];
    let gpu = request.gpu;
    let algorithms = job::resolve_request(&request).expect("the algorithms resolve");
    let observer = Arc::new(match kinds {
        Some(kinds) => GuiObserver::with_previews(kinds, HeatRamp::default()),
        None => GuiObserver::new(),
    });
    let outcome = job::spawn(
        request,
        algorithms,
        JobControl::new(),
        Arc::clone(&observer),
    )
    .join()
    .expect("the worker does not panic")
    .expect("the run completes");
    let FuseOutcome::Completed(report) = outcome else {
        panic!("the run was not cancelled");
    };
    let FuseProduct::Held(held) = &report.product else {
        panic!("a hold request reports a held checkpoint");
    };
    let backend = observer
        .backend_line()
        .expect("the worker names its backend");
    let expected = if gpu {
        "fusion backend: gpu"
    } else {
        "fusion backend: cpu"
    };
    assert!(backend.starts_with(expected), "{tag}: {backend}");
    assert!(
        report.gpu_fallback.is_none(),
        "{tag}: tiles fell back to the host: {:?}",
        report.gpu_fallback
    );
    Run {
        wall: report.wall_seconds,
        cost: report.preview_cost,
        megapixels: f64::from(report.output_width) * f64::from(report.output_height) / 1.0e6,
        frames: report.frame_count,
        composite: held.composite.clone(),
        dirs,
    }
}

/// Panics unless both composites hold the same samples. Compared band by band
/// so two full-size composites are never in memory at once.
fn assert_same_composite(plain: &Path, subscribed: &Path) {
    const BAND: u32 = 256;
    let open =
        |path: &Path| TiffStack::open_files(&[path.to_path_buf()]).expect("a composite opens");
    let (plain, subscribed) = (open(plain), open(subscribed));
    let (width, height) = plain.dimensions();
    assert_eq!(
        (width, height),
        subscribed.dimensions(),
        "the composites differ in size"
    );
    for y in (0..height).step_by(BAND as usize) {
        let band = Rect {
            x: 0,
            y,
            width,
            height: BAND.min(height - y),
        };
        let samples = |stack: &TiffStack| stack.read_tile(0, band).expect("a band reads").samples;
        assert!(
            samples(&plain) == samples(&subscribed),
            "the composite differs from row {y}: the subscription changed the result"
        );
    }
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

/// The measured runs of one leg.
struct Table {
    label: String,
    frames: usize,
    megapixels: f64,
    plain: Vec<f64>,
    subscribed: Vec<f64>,
    costs: Vec<PreviewCost>,
}

impl Table {
    fn limit(&self) -> f64 {
        (BUDGET_FRACTION * median(&self.plain)).min(BUDGET_SECONDS)
    }

    fn spread(&self) -> f64 {
        let max = self.plain.iter().copied().fold(f64::MIN, f64::max);
        let min = self.plain.iter().copied().fold(f64::MAX, f64::min);
        max - min
    }

    fn delta(&self) -> f64 {
        median(&self.subscribed) - median(&self.plain)
    }

    /// Each view's median in-run cost, in seconds.
    fn views(&self) -> [(&'static str, f64); 3] {
        let of = |pick: fn(&PreviewCost) -> Duration| {
            let seconds: Vec<f64> = self.costs.iter().map(|c| pick(c).as_secs_f64()).collect();
            median(&seconds)
        };
        [
            ("aligned frame", of(|c| c.aligned_frame)),
            ("composite", of(|c| c.composite)),
            ("heat", of(|c| c.heat)),
        ]
    }

    /// What broke a limit; empty when the leg is within budget.
    fn failures(&self) -> Vec<String> {
        let limit = self.limit();
        let mut failures: Vec<String> = self
            .views()
            .iter()
            .filter(|(_, cost)| *cost > limit)
            .map(|(view, cost)| format!("{view}: {cost:.3} s of its own work, limit {limit:.3} s"))
            .collect();
        if self.delta() > limit + self.spread() {
            failures.push(format!(
                "wall clock: +{:.3} s, limit {limit:.3} s plus spread {:.3} s",
                self.delta(),
                self.spread()
            ));
        }
        failures
    }

    fn print(&self) {
        let plain = median(&self.plain);
        let limit = self.limit();
        println!(
            "stage view budget [{}]: {} frames, {:.1} MP",
            self.label, self.frames, self.megapixels
        );
        println!("  unsubscribed walls {:?}", self.plain);
        println!("  subscribed walls   {:?}", self.subscribed);
        println!(
            "  median wall {plain:.3} s -> {:.3} s, delta {:+.3} s ({:+.2} %), unsubscribed spread {:.3} s, limit {limit:.3} s",
            median(&self.subscribed),
            self.delta(),
            100.0 * self.delta() / plain,
            self.spread()
        );
        for (view, cost) in self.views() {
            println!(
                "  {view:<13} own cost {cost:.3} s ({:.3} % of wall) {}",
                100.0 * cost / plain,
                if cost > limit { "REMOVE" } else { "keep" }
            );
        }
        let failures = self.failures();
        println!(
            "  verdict: {}",
            if failures.is_empty() {
                "within budget".to_string()
            } else {
                failures.join("; ")
            }
        );
    }
}

/// Measures `template` as one warm-up (when asked) and `pairs` interleaved
/// unsubscribed/subscribed pairs, alternating which of a pair runs first. A
/// pair whose composites differ, or whose cost accounting is dishonest, panics.
/// `vary` edits the subscribed run's request only.
fn measure(
    label: &str,
    template: &JobRequest,
    warmup: bool,
    pairs: usize,
    kinds: PreviewKinds,
    vary: Option<&dyn Fn(&mut JobRequest)>,
) -> Table {
    let clean = |run: Run| {
        run.dirs
            .iter()
            .for_each(|dir| _ = std::fs::remove_dir_all(dir))
    };
    if warmup {
        clean(run_once(template, &format!("{label}_warm"), None, None));
    }
    let mut table = Table {
        label: label.to_string(),
        frames: 0,
        megapixels: 0.0,
        plain: Vec::new(),
        subscribed: Vec::new(),
        costs: Vec::new(),
    };
    for pair in 0..pairs {
        let plain = |tag: &str| run_once(template, &format!("{label}_{pair}_{tag}"), None, None);
        let subscribed = |tag: &str| {
            run_once(
                template,
                &format!("{label}_{pair}_{tag}"),
                Some(kinds),
                vary,
            )
        };
        let (plain, subscribed) = if pair % 2 == 0 {
            let plain = plain("plain");
            (plain, subscribed("sub"))
        } else {
            let subscribed = subscribed("sub");
            (plain("plain"), subscribed)
        };
        assert_same_composite(&plain.composite, &subscribed.composite);
        assert_eq!(
            plain.cost,
            PreviewCost::default(),
            "an unsubscribed run reports no preview cost"
        );
        for (wanted, spent, view) in [
            (
                kinds.aligned_frame,
                subscribed.cost.aligned_frame,
                "aligned frame",
            ),
            (kinds.heat, subscribed.cost.heat, "heat"),
        ] {
            assert!(
                wanted || spent.is_zero(),
                "{view} was not requested but cost {spent:?}"
            );
        }
        assert!(
            !kinds.composite || !subscribed.cost.composite.is_zero(),
            "the composite view was requested and cost nothing"
        );
        table.frames = plain.frames;
        table.megapixels = plain.megapixels;
        table.plain.push(plain.wall);
        table.subscribed.push(subscribed.wall);
        table.costs.push(subscribed.cost);
        clean(plain);
        clean(subscribed);
    }
    table.print();
    table
}

/// A run request over `count` fixture frames, with the GUI's default
/// algorithms and flags.
fn template(name: &str, count: usize, gpu: bool) -> JobRequest {
    let mut request = common::controller_with_frames_from("mantis-512", name, count)
        .job_request_for(0)
        .expect("the project has a request");
    request.gpu = gpu;
    request
}

/// `frames` repeated in order up to `count` entries.
fn repeated_to(frames: &[PathBuf], count: usize) -> Vec<PathBuf> {
    frames.iter().cycle().take(count).cloned().collect()
}

/// The reference stack's `*.tif` frames in name order, without its stacked
/// result; `None` (after a `skip:` line) when the stack is not available.
fn reference_frames() -> Option<Vec<PathBuf>> {
    let dir = common::reference_stack_dir()?;
    let mut frames: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("the reference stack lists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("tif"))
                && !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("Mantis_Stacked"))
        })
        .collect();
    assert!(!frames.is_empty(), "no *.tif frames in {}", dir.display());
    frames.sort();
    Some(frames)
}

/// One reference-stack leg: `long` repeats the frames to a long run.
fn leg(label: &str, gpu: bool, long: bool) {
    let Some(mut frames) = reference_frames() else {
        return;
    };
    if long {
        frames = repeated_to(&frames, LONG_RUN_FRAMES);
    }
    let mut template = template(&format!("svb_{label}"), 1, gpu);
    template.frames = frames;
    if gpu {
        let (_, line) = job::resolve_backend(&template.fuse_id, true, template.max_vram);
        if !line.starts_with("fusion backend: gpu") {
            eprintln!("skip: no GPU backend for this run ({line})");
            return;
        }
    }
    let table = measure(label, &template, true, 3, ALL_VIEWS, None);
    let failures = table.failures();
    assert!(failures.is_empty(), "{label}: {}", failures.join("; "));
}

#[test]
fn stage_view_budget_harness_runs_on_the_fixture() {
    let template = template("svb_fixture", 3, false);
    let all = measure("fixture", &template, false, 1, ALL_VIEWS, None);
    let [(_, aligned), (_, composite), (_, heat)] = all.views();
    assert!(aligned > 0.0 && composite > 0.0 && heat > 0.0);

    let no_heat = PreviewKinds {
        heat: false,
        ..ALL_VIEWS
    };
    let without = measure("fixture_no_heat", &template, false, 1, no_heat, None);
    let [_, (_, composite), (_, heat)] = without.views();
    assert!(composite > 0.0);
    assert_eq!(
        heat, 0.0,
        "a run without the heat view reports no heat cost"
    );
}

#[test]
#[should_panic(expected = "the subscription changed the result")]
fn stage_view_budget_identity_check_rejects_a_differing_pair() {
    let template = template("svb_differs", 3, false);
    let coarser = |request: &mut JobRequest| {
        request
            .fuse_values
            .insert("levels".to_string(), ParamValue::Int(1));
    };
    measure("differs", &template, false, 1, ALL_VIEWS, Some(&coarser));
}

#[test]
fn stage_view_budget_long_run_list_repeats_the_frames() {
    let mut template = template("svb_repeat", 3, false);
    let source = template.frames.clone();
    template.frames = repeated_to(&source, 7);
    assert_eq!(template.frames[3], source[0]);
    assert_eq!(template.frames[6], source[0]);
    assert_eq!(
        measure("repeat", &template, false, 1, ALL_VIEWS, None).frames,
        7
    );
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn stage_view_budget_cpu_reference_stack() {
    leg("cpu_reference_stack", false, false);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn stage_view_budget_cpu_long_run() {
    leg("cpu_long_run", false, true);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn stage_view_budget_gpu_reference_stack() {
    leg("gpu_reference_stack", true, false);
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn stage_view_budget_gpu_long_run() {
    leg("gpu_long_run", true, true);
}
