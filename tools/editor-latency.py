#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""The editor latency figures of one `trace` GUI session.

Usage: tools/editor-latency.py --machine {laptop,workstation} [--json] TRACE
       tools/editor-latency.py --selftest
Exit:  0 report printed, 1 trace unusable (one line on stderr, no figures),
       2 usage error.

TRACE is the Chrome trace `FOCUSFORGE_TRACE_FILE` wrote for a session driven
by the procedure in tools/README.md. Every figure is derived from span
timestamps. A brush segment is followed from the input that caused it through
the worker to the first rendered frame that shows it; the stages are joined by
the stroke and segment fields of the spans and by their nesting on a thread.

`--selftest` builds synthetic traces with known answers and checks every
figure under both machine profiles and every refusal.
"""

import argparse
import json
import math
import os
import statistics
import subprocess
import sys
import tempfile

# machine -> (brush p95 ms, cost ratio, settle ms, frame rate Hz, switch ms)
LIMITS = {
    "laptop": (33.0, 1.5, 100.0, 30.0, 150.0),
    "workstation": (16.0, 1.5, 100.0, 60.0, 100.0),
}
STAGES = (
    ("queueing", "UI queueing"),
    ("worker_wait", "worker wait"),
    ("read_warp", "read and warp"),
    ("blend", "blend"),
    ("readback_wait", "readback wait"),
    ("hand_off", "hand-off"),
    ("install", "install"),
    ("copy", "copy"),
    ("overlay_lookup", "overlay lookup"),
    ("overlay_draw", "overlay draw"),
    ("upload", "upload"),
    ("frame", "rendered frame"),
)
LONG_STROKE_MS = 5000.0
LONG_STROKE_SEGMENTS = 10
GESTURE_GAP_MS = 500.0
MIN_GESTURE_ACTIONS = 3
MIN_GESTURE_MS = 250.0


class TraceError(Exception):
    """The trace cannot yield trustworthy figures."""


def number(value):
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, (int, float)):
        return value
    try:
        return int(str(value).strip('"'))
    except ValueError:
        return 0


def text(value):
    return str(value).strip('"')


class Span:
    def __init__(self, kind, tid, start, args):
        self.kind, self.tid, self.start, self.end = kind, tid, start, start
        self.stroke = number(args.get("stroke", 0))
        self.segment = number(args.get("segment", 0))
        self.generation = number(args.get("generation", 0))

    def dur(self):
        return self.end - self.start

    def holds(self, other):
        return (
            self.tid == other.tid
            and other.start >= self.start
            and other.end <= self.end
        )


def read_trace(raw):
    """(spans in start order, timing_summary args) of one closed trace array."""
    try:
        events = json.loads(raw)
    except ValueError:
        raise TraceError("the trace is not one closed JSON array") from None
    if not isinstance(events, list):
        raise TraceError("the trace is not one closed JSON array")
    open_spans, spans, summary = {}, [], None
    for event in events:
        if not isinstance(event, dict):
            continue
        tid, phase, at = event.get("tid"), event.get("ph"), event.get("ts", 0) / 1000.0
        if phase == "B":
            args = event.get("args") or {}
            span = None
            if event.get("name") == "timing":
                span = Span(text(args.get("kind", "")), tid, at, args)
            elif event.get("name") == "timing_summary":
                summary = args
            open_spans.setdefault(tid, []).append(span)
        elif phase == "E":
            stack = open_spans.get(tid)
            if not stack:
                raise TraceError("unbalanced B and E events on thread %s" % tid)
            span = stack.pop()
            if span is not None:
                span.end = at
                spans.append(span)
    if any(stack for stack in open_spans.values()):
        raise TraceError("unbalanced B and E events: a span never closed")
    if summary is None:
        raise TraceError("the trace has no timing_summary record")
    if number(summary.get("gui_exhausted_admissions", 0)) > 0:
        raise TraceError("the span budget was exhausted (gui_exhausted_admissions)")
    if text(summary.get("saturated", "false")).lower() == "true":
        raise TraceError("the timing counters saturated")
    spans.sort(key=lambda span: (span.start, -span.end))
    # The processing budget only feeds the readback-wait stage; once it is
    # spent that stage is a lower bound, so it is left out, not refused.
    return spans, number(summary.get("exhausted_admissions", 0)) > 0


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def first_at_or_after(spans, start, accept=lambda span: True):
    return next((s for s in spans if s.start >= start and accept(s)), None)


def stages_of(spans, by_kind, strokes, ui):
    """Per-segment stage times in ms, the input-to-visible times and counts."""
    inputs = [s for s in by_kind.get("gui_input", []) if s.tid == ui]
    frames = by_kind.get("gui_frame_rendered", [])
    rows, visible, unmatched = [], [], 0
    for stroke_id, segments in strokes.items():
        previous = None
        for seg in segments:
            if previous is None:
                before = [i for i in inputs if i.start <= seg.start]
                consumed = before[-1:]
            else:
                consumed = [i for i in inputs if previous.start < i.start <= seg.start]
            previous = seg
            job = next(
                (j for j in by_kind.get("gui_region_job", [])
                 if j.stroke == stroke_id and j.segment == seg.segment),
                None,
            )
            install = next(
                (i for i in by_kind.get("gui_region_install", [])
                 if i.stroke == stroke_id and i.segment == seg.segment),
                None,
            )
            if job is None or install is None:
                unmatched += len(consumed)
                continue
            blends = [b for b in by_kind.get("gui_region_blend", []) if job.holds(b)]
            waits = [
                w for w in by_kind.get("gpu_readback_wait", [])
                if any(b.holds(w) for b in blends)
            ]
            park = first_at_or_after(
                by_kind.get("gui_region_park", []), job.end, lambda p: p.tid == job.tid
            )
            settle = first_at_or_after(
                by_kind.get("gui_region_render_settle", []),
                park.start if park else job.end,
                lambda s: s.holds(install),
            )
            shown = first_at_or_after(
                by_kind.get("gui_composite_shown", []),
                install.start,
                lambda s: s.generation >= install.generation,
            )
            if park is None or settle is None or shown is None:
                unmatched += len(consumed)
                continue
            frame = first_at_or_after(frames, shown.start)
            if frame is None:
                unmatched += len(consumed)
                continue
            publish = None
            for candidate in by_kind.get("gui_canvas_publish", []):
                if candidate.tid == shown.tid and candidate.end <= shown.start:
                    publish = candidate

            def inside_publish(kind):
                return sum(
                    s.dur() for s in by_kind.get(kind, [])
                    if publish is not None and publish.holds(s)
                )

            draw = sum(
                s.dur() for s in by_kind.get("gui_overlay_draw", [])
                if s.tid == shown.tid and install.end <= s.start and s.end <= shown.start
            )
            blend = sum(b.dur() for b in blends)
            rows.append({
                "queueing": seg.start - consumed[0].start if consumed else 0.0,
                "worker_wait": max(0.0, job.start - seg.end),
                "read_warp": job.dur() - blend,
                "blend": blend,
                "readback_wait": sum(w.dur() for w in waits),
                "hand_off": settle.start - park.start,
                "install": install.dur(),
                "copy": inside_publish("gui_publish_copy"),
                "overlay_lookup": inside_publish("gui_publish_overlay_lookup"),
                "overlay_draw": draw,
                "upload": inside_publish("gui_publish_upload"),
                "frame": frame.start - shown.start,
                "job": job.dur(),
                "stroke": stroke_id,
                "segment": seg.segment,
            })
            visible.extend(frame.start - i.start for i in consumed)
    return rows, visible, unmatched


def worst_cost_ratio(rows, strokes, releases):
    worst = None
    for stroke_id, segments in strokes.items():
        release = releases.get(stroke_id)
        if release is None or len(segments) < LONG_STROKE_SEGMENTS:
            continue
        if release.start - segments[0].start < LONG_STROKE_MS:
            continue
        costs = [r["job"] for r in rows if r["stroke"] == stroke_id]
        if len(costs) < LONG_STROKE_SEGMENTS:
            continue
        tenth = max(1, len(costs) // 10)
        ratio = statistics.fmean(costs[-tenth:]) / statistics.fmean(costs[:tenth])
        worst = ratio if worst is None else max(worst, ratio)
    return worst


def settle_time(by_kind, releases):
    worst = None
    for stroke_id, release in releases.items():
        installs = [
            i for i in by_kind.get("gui_region_install", [])
            if i.stroke == stroke_id and i.segment == 0 and i.start >= release.start
        ]
        if not installs:
            continue
        reached = max(i.generation for i in installs)
        shown = first_at_or_after(
            by_kind.get("gui_composite_shown", []),
            release.start,
            lambda s: s.generation >= reached,
        )
        frame = shown and first_at_or_after(by_kind.get("gui_frame_rendered", []), shown.start)
        if frame is None:
            continue
        taken = frame.start - release.start
        worst = taken if worst is None else max(worst, taken)
    return worst


def gestures(actions):
    groups = []
    for action in actions:
        if groups and action.start - groups[-1][-1].start <= GESTURE_GAP_MS:
            groups[-1].append(action)
        else:
            groups.append([action])
    return groups


def frame_rates(by_kind, kind):
    """(kind, Hz, p95 interval ms) of every gesture of `kind`."""
    frames = by_kind.get("gui_frame_rendered", [])
    found = []
    for group in gestures(by_kind.get(kind, [])):
        if len(group) < MIN_GESTURE_ACTIONS:
            continue
        last = first_at_or_after(frames, group[-1].start)
        window = [f for f in frames if last is not None and group[0].start <= f.start <= last.start]
        if len(window) < 2 or window[-1].start == window[0].start:
            # A burst that ended inside one frame has no interval to measure;
            # a gesture that lasted and still drew one frame is a stall.
            if group[-1].start - group[0].start >= MIN_GESTURE_MS:
                found.append((kind, 0.0, 0.0))
            continue
        gaps = [b.start - a.start for a, b in zip(window, window[1:])]
        hertz = 1000.0 * (len(window) - 1) / (window[-1].start - window[0].start)
        found.append((kind, hertz, percentile(gaps, 0.95)))
    return found


def exact_detail_times(by_kind):
    times = []
    for group in gestures(by_kind.get("gui_action_zoom", [])):
        last = group[-1]
        publish = first_at_or_after(
            by_kind.get("gui_canvas_publish", []),
            last.start,
            lambda p: any(p.holds(u) for u in by_kind.get("gui_publish_upload", [])),
        )
        if publish is not None:
            times.append(publish.end - last.start)
    return max(times) if times else None


def switch_times(by_kind):
    steps = by_kind.get("gui_action_source_step", [])
    times = []
    for at, step in enumerate(steps):
        until = steps[at + 1].start if at + 1 < len(steps) else math.inf
        publish = first_at_or_after(
            by_kind.get("gui_frame_pane_publish", []),
            step.start,
            lambda p: p.start < until
            and any(p.holds(u) for u in by_kind.get("gui_publish_upload", [])),
        )
        frame = publish and first_at_or_after(by_kind.get("gui_frame_rendered", []), publish.end)
        if frame is not None:
            times.append(frame.start - step.start)
    return times


def analyze(raw, machine):
    """The report dict for the trace text `raw`; raises TraceError."""
    spans, partial = read_trace(raw)
    by_kind = {}
    for span in spans:
        by_kind.setdefault(span.kind, []).append(span)
    strokes = {}
    for seg in by_kind.get("gui_stroke_segment", []):
        if seg.stroke:
            strokes.setdefault(seg.stroke, []).append(seg)
    for segments in strokes.values():
        segments.sort(key=lambda s: s.segment)
    releases = {
        r.stroke: r for r in by_kind.get("gui_stroke_release", []) if r.stroke
    }
    for name, present in (
        ("stroke", strokes),
        ("release", releases),
        ("pan", by_kind.get("gui_action_pan")),
        ("zoom", by_kind.get("gui_action_zoom")),
        ("source switch", by_kind.get("gui_action_source_step")),
    ):
        if not present:
            raise TraceError("the trace has no %s" % name)
    long_strokes = [
        s for s, segs in strokes.items()
        if len(segs) >= LONG_STROKE_SEGMENTS
        and s in releases
        and releases[s].start - segs[0].start >= LONG_STROKE_MS
    ]
    if not long_strokes:
        raise TraceError("no stroke of at least 5 s and 10 segments")

    ui = by_kind["gui_stroke_segment"][0].tid
    rows, visible, unmatched = stages_of(spans, by_kind, strokes, ui)
    if not visible:
        raise TraceError("no input reached a rendered frame")
    ratio = worst_cost_ratio(rows, strokes, releases)
    settle = settle_time(by_kind, releases)
    if ratio is None or settle is None:
        raise TraceError("no stroke cost or release settle could be derived")
    rates = frame_rates(by_kind, "gui_action_pan") + frame_rates(by_kind, "gui_action_zoom")
    if not rates:
        raise TraceError("no pan or zoom gesture of at least %d actions" % MIN_GESTURE_ACTIONS)
    lowest = min(rates, key=lambda r: r[1])
    switches = switch_times(by_kind)
    if not switches:
        raise TraceError("no source step reached a rendered frame")

    brush_limit, ratio_limit, settle_limit, rate_limit, switch_limit = LIMITS[machine]
    figures = {
        "brush_p95_ms": (percentile(visible, 0.95), brush_limit, "<="),
        "cost_ratio": (ratio, ratio_limit, "<="),
        "settle_ms": (settle, settle_limit, "<="),
        "frame_rate_hz": (lowest[1], rate_limit, ">="),
        "switch_ms": (max(switches), switch_limit, "<="),
    }

    def verdict(value, limit, relation):
        return "pass" if (value <= limit if relation == "<=" else value >= limit) else "miss"

    report = {
        "machine": machine,
        "valid": True,
        "figures": {
            name: {
                "value": value, "limit": limit, "relation": relation,
                "result": verdict(value, limit, relation),
            }
            for name, (value, limit, relation) in figures.items()
        },
        "frame_rate_gesture": {"kind": lowest[0], "p95_interval_ms": lowest[2]},
        "switch_median_ms": statistics.median(switches),
        "exact_detail_ms": exact_detail_times(by_kind),
        "stages": {
            key: {"median_ms": None, "p95_ms": None}
            if partial and key == "readback_wait"
            else {
                "median_ms": statistics.median(r[key] for r in rows),
                "p95_ms": percentile([r[key] for r in rows], 0.95),
            }
            for key, _ in STAGES
        },
        "segments": len(rows),
        "inputs": len(visible),
        "inputs_excluded": unmatched,
        "strokes": len(strokes),
        "long_strokes": len(long_strokes),
    }
    return report


def render(report):
    figures = report["figures"]
    lines = ["machine: %s" % report["machine"], "valid: yes"]
    labels = (
        ("brush_p95_ms", "brush input-to-visible p95", "ms"),
        ("cost_ratio", "cost per segment, last tenth over first", "x"),
        ("settle_ms", "release to final state", "ms"),
        ("frame_rate_hz", "pan and zoom frame rate", "Hz"),
        ("switch_ms", "source switch to updated view", "ms"),
    )
    for key, label, unit in labels:
        fig = figures[key]
        lines.append(
            "%s: %.2f %s (limit %s %g %s) %s"
            % (label, fig["value"], unit, fig["relation"], fig["limit"], unit, fig["result"])
        )
    gesture = report["frame_rate_gesture"]
    lines.append(
        "  lowest gesture: %s, p95 frame interval %.1f ms"
        % (gesture["kind"], gesture["p95_interval_ms"])
    )
    lines.append("  source switch median: %.1f ms" % report["switch_median_ms"])
    exact = report["exact_detail_ms"]
    lines.append(
        "zoom to exact detail: %s (no limit)"
        % ("n/a" if exact is None else "%.1f ms" % exact)
    )
    lines.append("brush stages (median / p95, ms):")
    for key, label in STAGES:
        stage = report["stages"][key]
        if stage["median_ms"] is None:
            lines.append("  %-16s %8s %8s  (processing span budget spent)" % (label, "n/a", "n/a"))
        else:
            lines.append("  %-16s %8.2f %8.2f" % (label, stage["median_ms"], stage["p95_ms"]))
    lines.append(
        "segments: %d, inputs: %d (excluded %d), strokes: %d (long %d)"
        % (report["segments"], report["inputs"], report["inputs_excluded"],
           report["strokes"], report["long_strokes"])
    )
    return "\n".join(lines)


# ---------------------------------------------------------------- selftest


class Builder:
    """Synthetic trace spans on three threads: UI 1, worker 2, background 3."""

    UI, WORKER, BACKGROUND = 1, 2, 3

    def __init__(self):
        self.spans = []
        self.generation = 0

    def add(self, tid, kind, start, end=None, **args):
        end = start if end is None else end
        self.spans.append((tid, kind, start, end, args))

    def stroke(self, stroke_id, origin, count, gap, jobs):
        """`count` segments `gap` ms apart; segment k costs jobs(k) ms.

        Segment at t: input t-8, segment span [t, t+2], job from t+5 (blend 4
        ms with a 1 ms readback wait), park +1, settle +2 after the park
        (install 2 ms inside), publish 1 ms each of copy, lookup and upload,
        overlay draw 1 ms, shown 6 ms after the install, frame 3 ms later.
        Input to frame is 28 + job.
        """
        for k in range(1, count + 1):
            t, job = origin + gap * k, jobs(k)
            fields = {"stroke": stroke_id, "segment": k, "area": 100}
            self.add(self.UI, "gui_input", t - 8, t - 7)
            self.add(self.BACKGROUND, "gui_input", t - 3, t - 2)
            self.add(self.UI, "gui_stroke_segment", t, t + 2, **fields)
            start, end = t + 5, t + 5 + job
            self.add(self.WORKER, "gui_region_job", start, end, **fields)
            self.add(self.WORKER, "gui_region_blend", start + 2, start + 6)
            self.add(self.WORKER, "gpu_readback_wait", start + 3, start + 4)
            self.add(self.WORKER, "gui_region_park", end + 1, end + 2)
            self.add(self.BACKGROUND, "gui_region_park", t - 100, t - 99)
            self.install(stroke_id, k, end + 3, area=100)
            self.add(self.BACKGROUND, "gui_thumbnail", t, t + 30)

    def install(self, stroke_id, segment, settle_at, area=100):
        """A settle at `settle_at`, its install, the publish and the frame."""
        self.generation += 1
        gen = self.generation
        self.add(self.UI, "gui_region_render_settle", settle_at, settle_at + 4)
        self.add(
            self.UI, "gui_region_install", settle_at + 1, settle_at + 3,
            stroke=stroke_id, segment=segment, area=area, generation=gen,
        )
        base = settle_at + 3
        self.add(self.UI, "gui_canvas_publish", base + 1, base + 4, generation=gen)
        self.add(self.UI, "gui_publish_copy", base + 1, base + 2)
        self.add(self.UI, "gui_publish_overlay_lookup", base + 2, base + 3)
        self.add(self.UI, "gui_publish_upload", base + 3, base + 4)
        self.add(self.UI, "gui_overlay_draw", base + 4, base + 5)
        self.add(self.UI, "gui_composite_shown", base + 6, generation=gen)
        self.add(self.UI, "gui_frame_rendered", base + 9)

    def release(self, stroke_id, at, segments, settle_ms):
        self.add(self.UI, "gui_stroke_release", at, at + 1, stroke=stroke_id, segment=segments)
        self.generation += 1
        gen = self.generation
        shown = at + settle_ms - 3
        self.add(
            self.UI, "gui_region_install", at + 2, at + 4,
            stroke=stroke_id, segment=0, area=100, generation=gen,
        )
        self.add(self.UI, "gui_composite_shown", shown, generation=gen)
        self.add(self.UI, "gui_frame_rendered", at + settle_ms)

    def gesture(self, kind, origin, count, step, interval):
        """`count` actions `step` ms apart, a frame `interval` ms apart."""
        for i in range(count):
            self.add(self.UI, kind, origin + step * i, origin + step * i + 1)
        last = origin + step * (count - 1)
        t = origin + 2
        while t <= last + interval:
            self.add(self.UI, "gui_frame_rendered", t)
            t += interval
        return last

    def switch(self, at, delay):
        """A source step answered `delay` ms later by an uploading publish."""
        self.add(self.UI, "gui_action_source_step", at, at + 1)
        start = at + delay
        self.add(self.UI, "gui_frame_pane_publish", start, start + 10)
        self.add(self.UI, "gui_publish_upload", start + 2, start + 6)
        self.add(self.UI, "gui_frame_rendered", start + 15)

    def events(self, summary=None, drop=None, unbalance=False):
        per_tid = {}
        for index, (tid, kind, start, end, args) in enumerate(self.spans):
            if kind == drop:
                continue
            per_tid.setdefault(tid, []).append((start, -end, index, kind, end, args))
        out = []
        for tid, spans in per_tid.items():
            stack = []

            def close(until):
                while stack and stack[-1] <= until:
                    out.append((stack.pop(), tid, "E", None, None))

            for start, _, _, kind, end, args in sorted(spans, key=lambda s: s[:3]):
                close(start)
                out.append((start, tid, "B", kind, args))
                stack.append(end)
            close(math.inf)
        out.sort(key=lambda e: e[0])
        events = [
            {"ph": "B", "name": "timing_summary", "tid": 1, "ts": 0,
             "args": summary if summary is not None else {
                 "gui_exhausted_admissions": "0", "exhausted_admissions": "0",
                 "saturated": "false"}},
            {"ph": "E", "tid": 1, "ts": 0},
        ]
        for at, tid, phase, kind, args in out:
            event = {"ph": phase, "tid": tid, "ts": at * 1000.0}
            if phase == "B":
                event["name"] = "timing"
                event["args"] = dict(args, kind='"%s"' % kind)
            events.append(event)
        if unbalance:
            events.pop()
        return events

    def text(self, **kwargs):
        return json.dumps(self.events(**kwargs))


def sample_session():
    """Two strokes (20 and 12 segments, 6 s each), a pan, two zoom bursts and
    three source steps, with figures known by construction."""
    b = Builder()
    long_cost = lambda k: 20 if k >= 19 else 10
    b.stroke(1, 0, 20, 300, long_cost)
    b.release(1, 6500, 20, 60)
    b.stroke(2, 7000, 12, 500, lambda k: 12)
    b.release(2, 13500, 12, 40)
    b.gesture("gui_action_pan", 15000, 150, 20, 20)
    last_a = b.gesture("gui_action_zoom", 20000, 5, 100, 20)
    last_b = b.gesture("gui_action_zoom", 22000, 5, 100, 20)
    for last, delay in ((last_a, 30), (last_b, 40)):
        b.add(Builder.UI, "gui_canvas_publish", last + delay, last + delay + 5)
        b.add(Builder.UI, "gui_publish_upload", last + delay + 1, last + delay + 4)
    for at, delay in ((25000, 30), (26000, 40), (27000, 105)):
        b.switch(at, delay)
    return b


def close(left, right):
    return abs(left - right) < 1e-6


def expect_error(raw, fragment, label):
    try:
        analyze(raw, "laptop")
    except TraceError as error:
        assert fragment in str(error), "%s: %s" % (label, error)
        return
    raise AssertionError("%s: the trace was accepted" % label)


def selftest():
    session = sample_session()
    laptop = analyze(session.text(), "laptop")
    station = analyze(session.text(), "workstation")
    fig = lambda report, key: report["figures"][key]
    # Input to frame is 28 + job: 18 x 38 and 2 x 48 on stroke 1, 12 x 40 on
    # stroke 2; the nearest-rank p95 of 32 inputs is the 31st.
    assert close(fig(laptop, "brush_p95_ms")["value"], 48.0), laptop
    assert close(fig(laptop, "cost_ratio")["value"], 2.0), laptop
    assert close(fig(laptop, "settle_ms")["value"], 60.0), laptop
    assert close(fig(laptop, "frame_rate_hz")["value"], 50.0), laptop
    assert close(laptop["frame_rate_gesture"]["p95_interval_ms"], 20.0), laptop
    assert close(fig(laptop, "switch_ms")["value"], 120.0), laptop
    assert close(laptop["switch_median_ms"], 55.0), laptop
    assert close(laptop["exact_detail_ms"], 45.0), laptop
    assert (laptop["segments"], laptop["inputs"], laptop["inputs_excluded"]) == (32, 32, 0)
    assert (laptop["strokes"], laptop["long_strokes"]) == (2, 2)
    results = lambda report: {k: v["result"] for k, v in report["figures"].items()}
    assert results(laptop) == {
        "brush_p95_ms": "miss", "cost_ratio": "miss", "settle_ms": "pass",
        "frame_rate_hz": "pass", "switch_ms": "pass",
    }, results(laptop)
    assert results(station) == {
        "brush_p95_ms": "miss", "cost_ratio": "miss", "settle_ms": "pass",
        "frame_rate_hz": "miss", "switch_ms": "miss",
    }, results(station)
    assert fig(station, "brush_p95_ms")["limit"] == 16.0
    assert fig(laptop, "brush_p95_ms")["limit"] == 33.0
    stages = laptop["stages"]
    for key, median in (
        ("queueing", 8.0), ("worker_wait", 3.0), ("read_warp", 6.0), ("blend", 4.0),
        ("readback_wait", 1.0), ("hand_off", 2.0), ("install", 2.0), ("copy", 1.0),
        ("overlay_lookup", 1.0), ("overlay_draw", 1.0), ("upload", 1.0), ("frame", 3.0),
    ):
        assert close(stages[key]["median_ms"], median), (key, stages[key])
    assert close(stages["read_warp"]["p95_ms"], 16.0), stages["read_warp"]
    assert "pass" in render(laptop) and "miss" in render(laptop)

    # Refusals: each names its problem and yields no report.
    expect_error("{}", "not one closed JSON array", "an object")
    expect_error(session.text()[:-3], "not one closed JSON array", "a cut file")
    events = session.events()
    expect_error(json.dumps(events[2:]), "timing_summary", "no summary")
    for field, value in (("gui_exhausted_admissions", "3"), ("saturated", "true")):
        summary = {"gui_exhausted_admissions": "0", "exhausted_admissions": "0",
                   "saturated": "false", field: value}
        expect_error(session.text(summary=summary), "budget" if field != "saturated" else "saturated", field)
    spent = analyze(session.text(summary={
        "gui_exhausted_admissions": "0", "exhausted_admissions": "5", "saturated": "false"}), "laptop")
    assert spent["stages"]["readback_wait"]["median_ms"] is None, spent["stages"]
    assert spent["stages"]["blend"]["median_ms"] is not None, spent["stages"]
    assert "n/a" in render(spent)
    expect_error(session.text(unbalance=True), "unbalanced", "a missing E")
    for kind, name in (
        ("gui_stroke_segment", "stroke"), ("gui_stroke_release", "release"),
        ("gui_action_pan", "pan"), ("gui_action_zoom", "zoom"),
        ("gui_action_source_step", "source switch"),
    ):
        expect_error(session.text(drop=kind), "no %s" % name, name)
    short = Builder()
    short.stroke(1, 0, 12, 300, lambda k: 10)
    short.release(1, 4000, 12, 60)
    short.gesture("gui_action_pan", 5000, 10, 20, 20)
    short.gesture("gui_action_zoom", 6000, 3, 100, 20)
    short.switch(7000, 30)
    expect_error(short.text(), "no stroke of at least 5 s and 10 segments", "a short stroke")
    lone = Builder()
    lone.stroke(1, 0, 12, 300, lambda k: 10)
    lone.release(1, 6000, 12, 60)
    lone.gesture("gui_action_pan", 7000, 1, 20, 20)
    lone.gesture("gui_action_zoom", 8000, 2, 100, 20)
    lone.switch(9000, 30)
    expect_error(lone.text(), "no pan or zoom gesture", "single-action gestures")
    stall = Builder()
    stall.stroke(1, 0, 12, 300, lambda k: 10)
    stall.release(1, 6000, 12, 60)
    stall.gesture("gui_action_pan", 7000, 10, 100, 5000)
    stall.gesture("gui_action_zoom", 8000, 10, 100, 20)
    stall.switch(9000, 30)
    report = analyze(stall.text(), "laptop")
    assert report["figures"]["frame_rate_hz"]["value"] <= 1.0, report["figures"]

    # The command line: exit 1 with one stderr line and no figure, exit 2 on usage.
    with tempfile.TemporaryDirectory() as folder:
        path = os.path.join(folder, "trace.json")
        script = os.path.abspath(__file__)
        run = lambda *args: subprocess.run(
            [sys.executable, "-I", script, *args], capture_output=True, text=True)
        with open(path, "w") as handle:
            handle.write("[")
        bad = run("--machine", "laptop", path)
        assert bad.returncode == 1 and bad.stdout == "", bad
        assert len(bad.stderr.strip().splitlines()) == 1, bad.stderr
        with open(path, "w") as handle:
            handle.write(session.text())
        good = run("--machine", "workstation", "--json", path)
        assert good.returncode == 0, good.stderr
        assert json.loads(good.stdout)["figures"]["frame_rate_hz"]["result"] == "miss"
        assert run("--machine", "desktop", path).returncode == 2
        assert run(path).returncode == 2
        assert run("--machine", "laptop").returncode == 2
    print("editor-latency selftest: ok")


def main():
    parser = argparse.ArgumentParser(description="Editor latency figures of one trace.")
    parser.add_argument("trace", nargs="?", help="the Chrome trace file of one session")
    parser.add_argument("--machine", choices=sorted(LIMITS), help="limit profile")
    parser.add_argument("--json", action="store_true", help="print one JSON object")
    parser.add_argument("--selftest", action="store_true", help="check the analyzer itself")
    args = parser.parse_args()
    if args.selftest:
        selftest()
        return 0
    if args.trace is None or args.machine is None:
        parser.error("a TRACE and --machine are required")
    try:
        with open(args.trace, encoding="utf-8") as handle:
            report = analyze(handle.read(), args.machine)
    except (OSError, TraceError) as error:
        print("editor-latency: %s" % error, file=sys.stderr)
        return 1
    print(json.dumps(report, indent=2) if args.json else render(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
