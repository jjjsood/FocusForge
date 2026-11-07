# SPDX-License-Identifier: GPL-3.0-or-later
"""Dense local focus-score variants used by the comparison command."""

from __future__ import annotations

from collections.abc import Callable
import hashlib
import json
from pathlib import Path

import numpy as np


Score = Callable[[np.ndarray, np.ndarray, int], np.ndarray]

_REGISTERED_CONFIGURATION_SHA256 = "79b83f91e93fe35992b69926801927f176dffb6cb76c90e5b6ad78555e9529dc"


def luminance(plane: np.ndarray) -> np.ndarray:
    """Return encoded-sample luminance as the channel mean."""
    if plane.ndim != 3 or plane.shape[2] < 1:
        raise ValueError("plane must be HxWxC")
    return plane.astype(np.float64, copy=False).mean(axis=2, dtype=np.float64)


def _luminance_sum(plane: np.ndarray) -> tuple[np.ndarray, float]:
    """Return exact channel sums and the factor that restores mean-luma units."""
    if plane.ndim != 3 or plane.shape[2] < 1:
        raise ValueError("plane must be HxWxC")
    return plane.astype(np.float64, copy=False).sum(axis=2, dtype=np.float64), float(plane.shape[2])


def _validate_radius(radius: int) -> None:
    if not isinstance(radius, int) or isinstance(radius, bool) or radius < 1:
        raise ValueError("score radius must be a positive integer")


def _sum(plane: np.ndarray, radius: int) -> np.ndarray:
    """Return edge-clamped square sums in f64."""
    padded = np.pad(plane.astype(np.float64, copy=False), radius, mode="edge")
    prefix = np.pad(padded, ((1, 0), (1, 0))).cumsum(axis=0).cumsum(axis=1)
    width = 2 * radius + 1
    return prefix[width:, width:] - prefix[:-width, width:] - prefix[width:, :-width] + prefix[:-width, :-width]


def _weighted_sum(plane: np.ndarray, radius: int, axis: int) -> np.ndarray:
    """Return local centered-coordinate times sample sums in f64."""
    source = plane.astype(np.float64, copy=False)
    padded = np.pad(source, radius, mode="edge")
    height, width = source.shape
    result = np.zeros_like(source, dtype=np.float64)
    for offset in range(-radius, radius + 1):
        if axis == 1:
            result += offset * padded[radius:radius + height, radius + offset:radius + offset + width]
        else:
            result += offset * padded[radius + offset:radius + offset + height, radius:radius + width]
    padded_result = np.pad(result, ((radius, radius), (radius, radius)), mode="edge")
    summed = np.zeros_like(source, dtype=np.float64)
    for offset in range(-radius, radius + 1):
        if axis == 1:
            summed += padded_result[radius + offset:radius + offset + height, radius:radius + width]
        else:
            summed += padded_result[radius:radius + height, radius + offset:radius + offset + width]
    return summed


def detrended_std(gaussian: np.ndarray, radius: int) -> np.ndarray:
    """Return dense population residual standard deviation after a local plane fit."""
    _validate_radius(radius)
    if not np.isfinite(gaussian).all():
        raise ValueError("score input must be finite")
    values, channels = _luminance_sum(gaussian)
    count = float((2 * radius + 1) ** 2)
    coordinate_energy = float((2 * radius + 1) * sum(i * i for i in range(-radius, radius + 1)))
    total = _sum(values, radius)
    square_total = _sum(values * values, radius)
    x_total = _weighted_sum(values, radius, 1)
    y_total = _weighted_sum(values, radius, 0)
    residual = square_total - total * total / count - (x_total * x_total + y_total * y_total) / coordinate_energy
    return (np.sqrt(np.maximum(residual / count, 0.0)) / channels).astype(np.float32)


def plain_variance(gaussian: np.ndarray, radius: int) -> np.ndarray:
    """Return dense population luminance variance without detrending."""
    _validate_radius(radius)
    if not np.isfinite(gaussian).all():
        raise ValueError("score input must be finite")
    values, channels = _luminance_sum(gaussian)
    count = float((2 * radius + 1) ** 2)
    return (np.maximum(_sum(values * values, radius) / count - (_sum(values, radius) / count) ** 2, 0.0) / channels**2).astype(np.float32)


def current(_gaussian: np.ndarray, detail: np.ndarray, radius: int) -> np.ndarray:
    """Use the current all-channel Laplacian score."""
    from .fusion import score_plane
    return score_plane(detail, radius)


def luma_laplacian(_gaussian: np.ndarray, detail: np.ndarray, radius: int) -> np.ndarray:
    """Use the current Laplacian score on luminance only."""
    from .fusion import score_plane
    return score_plane(luminance(detail)[..., None].astype(np.float32), radius)


def detrended(gaussian: np.ndarray, _detail: np.ndarray, radius: int) -> np.ndarray:
    """Use plane-detrended luminance population standard deviation."""
    return detrended_std(gaussian, radius)


def variance(gaussian: np.ndarray, _detail: np.ndarray, radius: int) -> np.ndarray:
    """Use plain luminance population variance."""
    return plain_variance(gaussian, radius)


def score_by_name(name: str) -> Score:
    """Return one registered score function."""
    scores = {"current": current, "detrended-std": detrended, "luma-laplacian": luma_laplacian, "plain-variance": variance}
    try:
        return scores[name]
    except KeyError as error:
        raise ValueError(f"unknown focus score: {name}") from error


def adaptive_detrended(radii: tuple[int, ...]) -> Score:
    """Select the largest detrended score while keeping smaller-radius ties."""
    if not radii or tuple(sorted(set(radii))) != radii or radii[0] < 1:
        raise ValueError("adaptive radii must be sorted positive unique integers")
    def score(gaussian: np.ndarray, _detail: np.ndarray, _radius: int) -> np.ndarray:
        best = detrended_std(gaussian, radii[0])
        for radius in radii[1:]:
            best = np.maximum(best, detrended_std(gaussian, radius))
        return best
    return score


def self_checks() -> list[str]:
    """Assert cheap algebraic invariants for the registered score variants."""
    from .fusion import NO_SOURCE, weighted_average

    ramp = np.add.outer(np.arange(17, dtype=np.float32), 2 * np.arange(19, dtype=np.float32))[..., None]
    textured = ramp + ((np.indices(ramp.shape[:2]).sum(axis=0) % 2) * 7).astype(np.float32)[..., None]
    flat = np.full((17, 19, 3), 1234, dtype=np.float32)
    interior = (slice(3, -3), slice(3, -3))
    broken_detrending = np.sqrt(plain_variance(ramp, 3), dtype=np.float32)
    broken_variance = plain_variance(textured, 3)
    mono = textured[..., :1]
    rgb = np.repeat(mono, 3, axis=2)
    high_ramp = (np.add.outer(np.arange(17, dtype=np.float32) * 4096, np.arange(19, dtype=np.float32) * 2048)[..., None]
                 + np.array([100, 101, 101], dtype=np.float32))
    high_constant = np.empty((544, 544, 3), dtype=np.float32)
    high_constant[...] = [65535, 65534, 65535]
    window = np.pad(ramp[..., 0], 3, mode="edge")[:7, :7].astype(np.float64)
    yy, xx = np.indices(window.shape, dtype=np.float64)
    fit = np.column_stack((np.ones(window.size), xx.ravel() - 3, yy.ravel() - 3))
    direct_edge = np.sqrt(np.mean((window.ravel() - fit @ np.linalg.lstsq(fit, window.ravel(), rcond=None)[0]) ** 2))
    invalid_radius = []
    for radius in (0, -1, True, 1.0):
        try:
            detrended_std(textured, radius)  # type: ignore[arg-type]
        except ValueError:
            invalid_radius.append(radius)
    nonfinite = textured.copy()
    nonfinite[0, 0, 0] = np.nan
    try:
        plain_variance(nonfinite, 3)
    except ValueError:
        nonfinite_rejected = True
    else:
        nonfinite_rejected = False
    flat_stack = np.stack((np.full((64, 64, 3), 100, dtype=np.float32), np.full((64, 64, 3), 300, dtype=np.float32)))
    flat_coverage = np.ones(flat_stack.shape[:3], dtype=bool)
    flat_coverage[0, 0, 0] = False
    flat_coverage[:, 0, 63] = False
    flat_result = weighted_average(flat_stack, flat_coverage, score=detrended)
    checks: list[tuple[str, bool]] = [
        ("detrended-constant", bool(np.array_equal(detrended_std(flat, 3), np.zeros((17, 19), dtype=np.float32)))),
        ("detrended-ramp", bool(np.max(detrended_std(ramp, 3)[interior]) == 0.0)),
        ("detrended-unequal-rgb-ramp", bool(np.max(detrended_std(high_ramp, 3)[interior]) == 0.0)),
        ("detrended-high-constant", bool(np.max(detrended_std(high_constant, 16)) == 0.0)),
        ("variance-ramp", bool(np.all(plain_variance(ramp, 3)[interior] > 0.0))),
        ("detrended-plane-invariant", bool(np.allclose(detrended_std(textured, 3)[interior], detrended_std(textured + ramp, 3)[interior], rtol=0, atol=1e-5))),
        ("detrended-contrast", bool(np.allclose(detrended_std(textured * 2, 3)[interior], detrended_std(textured, 3)[interior] * 2, rtol=0, atol=1e-5))),
        ("edge-direct-fit", bool(np.isclose(detrended_std(ramp, 3)[0, 0], direct_edge, rtol=0, atol=1e-5))),
        ("mono-rgb", bool(np.array_equal(detrended_std(mono, 3), detrended_std(rgb, 3)))),
        ("radius-rejected", len(invalid_radius) == 4),
        ("nonfinite-rejected", nonfinite_rejected),
        ("flat-fallback", bool(flat_result.composite[32, 32, 0] == 200 and flat_result.composite[0, 0, 0] == 300
                                and flat_result.composite[0, 63, 0] == 0 and flat_result.winners[0, 63] == NO_SOURCE)),
        ("broken-detrending-rejected", bool(np.max(broken_detrending[interior]) > 0.0)),
        ("broken-variance-rejected", bool(not np.allclose(plain_variance(textured * 2, 3)[interior], broken_variance[interior] * 2, rtol=0, atol=1e-5))),
    ]
    failed = [name for name, ok in checks if not ok]
    if failed:
        raise AssertionError(", ".join(failed))
    return [name for name, _ok in checks]


def radius10_support_checks() -> list[dict[str, object]]:
    """Assert the registered radius-10 diagnostic and guarded support boundaries."""
    from .fusion import FusionResult, depth_map_replay, detail, eligible, score_plane
    from . import metrics

    size, center = 51, 25
    yy, xx = np.indices((size, size), dtype=np.float32)
    clean = (yy + xx)[..., None]
    changed = clean.copy()
    changed[center, center, 0] = 50000
    frames_clean = np.stack((clean, clean + ((xx % 3) * 2)[..., None]))
    frames_changed = frames_clean.copy()
    frames_changed[:, center, center, 0] += np.array((50000, 30000), dtype=np.float32)
    raw_clean = detrended_std(frames_clean[0], 10)
    raw_changed = detrended_std(frames_changed[0], 10)
    smooth_clean = score_plane(raw_clean[..., None], 8)
    smooth_changed = score_plane(raw_changed[..., None], 8)
    current_raw_clean = score_plane(detail(frames_clean[0]), 4)
    current_raw_changed = score_plane(detail(frames_changed[0]), 4)
    current_smooth_clean = score_plane(current_raw_clean[..., None], 8)
    current_smooth_changed = score_plane(current_raw_changed[..., None], 8)
    coverage = np.ones((size, size), dtype=bool)
    coverage[center, center] = False
    e16, e18 = eligible(coverage, 16), eligible(coverage, 18)
    candidate16 = depth_map_replay(frames_clean, np.stack((coverage, coverage)), 10, 8, 16, detrended)
    candidate18 = depth_map_replay(frames_clean, np.stack((coverage, coverage)), 10, 8, 18, detrended)
    changed16 = depth_map_replay(frames_changed, np.stack((coverage, coverage)), 10, 8, 16, detrended)
    changed18 = depth_map_replay(frames_changed, np.stack((coverage, coverage)), 10, 8, 18, detrended)
    current16 = depth_map_replay(frames_clean, np.stack((coverage, coverage)), 4, 8, 16, current)
    current18 = depth_map_replay(frames_clean, np.stack((coverage, coverage)), 4, 8, 18, current)
    current_changed16 = depth_map_replay(frames_changed, np.stack((coverage, coverage)), 4, 8, 16, current)
    current_changed18 = depth_map_replay(frames_changed, np.stack((coverage, coverage)), 4, 8, 18, current)
    probes: list[tuple[str, bool, dict[str, object]]] = []
    raw_delta = np.abs(raw_clean - raw_changed)
    current_raw_delta = np.abs(current_raw_clean - current_raw_changed)
    unsafe = e16 & ~e18
    masks = np.stack((coverage, coverage))
    mixed_region_pixels = int((masks.any(axis=0) & ~masks.all(axis=0)).sum())
    def border_metric(result: object, source_frames: np.ndarray) -> float | None:
        if not mixed_region_pixels:
            return None
        source = metrics.gff.ArraySource(np.rint(source_frames).clip(0, 65535).astype(np.uint16), masks)
        return float(metrics.gff.border_pull(result.composite, source))
    candidate16_before, candidate16_after = border_metric(candidate16, frames_clean), border_metric(changed16, frames_changed)
    current16_before, current16_after = border_metric(current16, frames_clean), border_metric(current_changed16, frames_changed)
    candidate16_border = None if candidate16_before is None or candidate16_after is None else candidate16_after - candidate16_before
    current16_border = None if current16_before is None or current16_after is None else current16_after - current16_before
    arms = (("candidate-e16", candidate16, changed16, e16, raw_delta, smooth_clean, smooth_changed),
            ("candidate-e18", candidate18, changed18, e18, raw_delta, smooth_clean, smooth_changed),
            ("current-r4-e16", current16, current_changed16, e16, current_raw_delta, current_smooth_clean, current_smooth_changed),
            ("current-r4-e18", current18, current_changed18, e18, current_raw_delta, current_smooth_clean, current_smooth_changed))
    for distance in (17, 18, 19):
        point = (center, center + distance)
        for arm, clean_result, changed_result, support, arm_raw_delta, arm_smooth_clean, arm_smooth_changed in arms:
            output_delta = np.abs(clean_result.composite.astype(np.int64) - changed_result.composite.astype(np.int64))
            values = {"arm": arm, "distance": distance, "eligible": bool(support[point]),
                      "raw_score_delta": float(arm_raw_delta[point[0] - 8:point[0] + 9, point[1] - 8:point[1] + 9].max()),
                      "smoothed_score_delta": float(abs(arm_smooth_clean[point] - arm_smooth_changed[point])),
                      "winner_changed": bool(clean_result.winners[point] != changed_result.winners[point]),
                      "output_delta": int(output_delta[point].max()),
                      "winner_changes": int((clean_result.winners != changed_result.winners).sum()),
                      "output_max_delta": int(output_delta.max()),
                      "unsupported_support_count": int(unsafe.sum()) if arm == "candidate-e16" else 0,
                      "border_contamination_delta": candidate16_border if arm == "candidate-e16" else current16_border if arm == "current-r4-e16" else None,
                      "border_contamination_region_pixels": mixed_region_pixels,
                      "border_contamination_reason": None if mixed_region_pixels else "unavailable: no mixed-validity source region",
                      "output_dependency_mean_delta": float(output_delta[unsafe].mean()) if unsafe.any() else 0.0}
            candidate_probe = arm == "candidate-e16" and distance in (17, 18)
            candidate_safe = arm == "candidate-e18" and distance == 19
            expected_eligible = distance == 19 or arm.endswith("e16")
            name = f"radius10-distance{distance}" if candidate_probe else f"radius10-{arm}-distance{distance}"
            probes.append((name, bool(values["eligible"] == expected_eligible and ((values["smoothed_score_delta"] > 0) if candidate_probe else True)
                                                                        and ((values["smoothed_score_delta"] == 0 and not values["winner_changed"] and values["output_delta"] == 0) if candidate_safe else True)), values))
    def safe_equal(clean_result: object, changed_result: object) -> bool:
        return bool(np.array_equal(clean_result.winners[e18], changed_result.winners[e18])
                    and np.array_equal(clean_result.composite[e18], changed_result.composite[e18]))
    broken = FusionResult(changed18.composite.copy(), changed18.winners.copy(), changed18.eligible)
    broken.composite[center, center + 19, 0] += np.uint16(1)
    checks = probes + [
        ("radius10-safe-support", bool(safe_equal(candidate18, changed18) and safe_equal(current18, current_changed18) and not safe_equal(candidate18, broken)),
         {"unsupported_support_count": int(unsafe.sum()), "candidate_winner_changes": int((candidate18.winners != changed18.winners).sum()),
          "current_winner_changes": int((current18.winners != current_changed18.winners).sum()),
          "safe_output_max_delta": int(np.abs(candidate18.composite.astype(np.int64) - changed18.composite.astype(np.int64))[e18].max()),
          "current_safe_output_max_delta": int(np.abs(current18.composite.astype(np.int64) - current_changed18.composite.astype(np.int64))[e18].max()),
          "border_contamination_delta": candidate16_border, "current_border_contamination_delta": current16_border,
          "border_contamination_region_pixels": mixed_region_pixels,
          "border_contamination_reason": None if mixed_region_pixels else "unavailable: no mixed-validity source region",
          "broken_safe_output_rejected": True}),
        ("radius10-matched-controls", bool(np.array_equal(candidate16.eligible, current16.eligible)
                                             and np.array_equal(candidate18.eligible, current18.eligible)),
         {"diagnostic_winner_changes": int((candidate16.winners != changed16.winners).sum()),
          "current_diagnostic_winner_changes": int((current16.winners != current_changed16.winners).sum()),
          "current_safe_winner_changes": int((current18.winners != current_changed18.winners).sum())}),
        ("diagnostic-cannot-advance", bool(not can_advance("depth-map-detrended-std-r10-e16")
                                              and can_advance("depth-map-detrended-std-r10-e18")), {}),
    ]
    failed = [name for name, ok, _values in checks if not ok]
    if failed:
        raise AssertionError(", ".join(failed))
    return [{"name": name, **values} for name, _ok, values in checks]


def can_advance(configuration_id: str) -> bool:
    """Keep the only registered unsafe boundary row out of advancement."""
    return configuration_id != "depth-map-detrended-std-r10-e16"


def _sha256_path(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _validate_baseline_inputs(baseline: Path) -> None:
    """Bind every replay input and retained output before candidate work begins."""
    try:
        rows = json.loads(baseline.read_text())
    except json.JSONDecodeError as error:
        raise ValueError("baseline records are not valid JSON") from error
    if not isinstance(rows, list):
        raise ValueError("baseline records are not a list")
    hashes: dict[Path, str] = {}
    def bound(path: Path, digest: str) -> bool:
        if not path.is_file():
            return False
        actual = hashes.get(path)
        if actual is None:
            actual = _sha256_path(path)
            hashes[path] = actual
        return actual == digest
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("input_sha256"), dict):
            raise ValueError("baseline row lacks input hashes")
        paths = row.get("actual_source_frames") or [str(Path(str(row.get("case", ""))) / "frames" / name) for name in row["input_sha256"]]
        expected = row.get("actual_source_sha256") or row["input_sha256"]
        if not isinstance(paths, list) or not isinstance(expected, dict):
            raise ValueError("baseline source bindings are invalid")
        for path_text in paths:
            path = Path(str(path_text))
            if not isinstance(expected.get(path.name), str) or not bound(path, expected[path.name]):
                raise ValueError("baseline source input is missing or stale")
        for path_key, digest_key in (("composite", "composite_sha256"), ("zmap", "zmap_sha256")):
            path_text, digest = row.get(path_key), row.get(digest_key)
            if not isinstance(path_text, str) or not isinstance(digest, str) or not bound(Path(path_text), digest):
                raise ValueError("baseline retained artifact is missing or stale")
        coverage_path, coverage_digest = row.get("actual_coverage"), row.get("actual_coverage_sha256")
        if row.get("actual_source_frames") and (not isinstance(coverage_path, str) or not isinstance(coverage_digest, str)):
            raise ValueError("aligned baseline row lacks coverage binding")
        if coverage_path is not None or coverage_digest is not None:
            if not isinstance(coverage_path, str) or not isinstance(coverage_digest, str) or not bound(Path(coverage_path), coverage_digest):
                raise ValueError("baseline coverage artifact is missing or stale")


def validate_command(registration: Path, runs: Path, allowances: Path, source: str | None,
                     prepared: Path | None, rect: str | None) -> dict[str, object]:
    """Reject unbound or incomplete comparison inputs before output creation."""
    if not registration.is_file():
        raise ValueError("--registration must name the committed comparison registration")
    if not (runs / "baseline.json").is_file() or not allowances.is_file():
        raise ValueError("--runs baseline.json and --allowances are required before comparison")
    registration_bytes = registration.read_bytes()
    try:
        document = json.loads(registration_bytes)
    except json.JSONDecodeError as error:
        raise ValueError("registration is not valid JSON") from error
    configurations = document.get("configurations")
    controls = document.get("matched_controls")
    payload = {"configurations": configurations, "matched_controls": controls}
    configuration_hash = hashlib.sha256(json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    if document.get("schema_version") != 1 or not isinstance(configurations, list) or not isinstance(controls, list) or configuration_hash != _REGISTERED_CONFIGURATION_SHA256:
        raise ValueError("registration has an unknown comparison configuration")
    execution = document.get("execution_module_sha256")
    package = Path(__file__).parent
    expected = {f"spikes/stacking_experiments/{name}": hashlib.sha256((package / name).read_bytes()).hexdigest()
                for name in ("__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py", "focus_score.py")}
    if not isinstance(execution, dict) or set(execution) != set(expected) or any(execution.get(name) != digest for name, digest in expected.items()):
        raise ValueError("registration does not bind the current comparison implementation")
    if hashlib.sha256((runs / "baseline.json").read_bytes()).hexdigest() != document.get("baseline_sha256"):
        raise ValueError("baseline records do not match the registration")
    _validate_baseline_inputs(runs / "baseline.json")
    if hashlib.sha256(allowances.read_bytes()).hexdigest() != document.get("allowances_sha256"):
        raise ValueError("allowances do not match the registration")
    if source is None:
        if prepared is not None or rect is not None:
            raise ValueError("--prepared and --rect require --source stack")
    else:
        if prepared is None:
            raise ValueError("--prepared preparation-manifest.json is required for --source stack")
        if not prepared.is_file():
            raise ValueError("prepared preparation manifest is missing")
        if rect not in (None, "1878,1056,1024,1024"):
            raise ValueError("--rect must equal the registered stack rectangle")
        expected_preparation = document.get("real_preparation", {})
        if hashlib.sha256(prepared.read_bytes()).hexdigest() != expected_preparation.get("manifest_sha256"):
            raise ValueError("prepared manifest does not match the registration")
        try:
            manifest = json.loads(prepared.read_text())
        except json.JSONDecodeError as error:
            raise ValueError("prepared manifest is not valid JSON") from error
        if (manifest.get("alignment_report_sha256") != expected_preparation.get("report_sha256")
                or manifest.get("raw_inputs") != expected_preparation.get("raw_inputs")
                or manifest.get("aligned_inputs") != expected_preparation.get("aligned_inputs")):
            raise ValueError("prepared report or inputs do not match the registration")
        from .runner import _validate_preparation
        _validate_preparation(prepared, source)
    document["_validated_registration_sha256"] = hashlib.sha256(registration_bytes).hexdigest()
    return document
