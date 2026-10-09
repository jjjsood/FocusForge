"""Command interface for fixed stack-experiment controls."""

from __future__ import annotations

import argparse
import math
import hashlib
import json
import os
from pathlib import Path
import sys
import time

import numpy as np

from . import coefficient_shrinkage, consistency, focus_score, fusion, index_field
from .protocol import FAMILIES, HOLDOUT_SEEDS, TRAINING_SEEDS, SeedTriple, registration, require_seed, validate_registration
from .scenes import family_members, render_family
from .runner import _twin_disagreements, _validate_preparation, baseline_call_count, case_split, focus_configuration_checks, focus_support_records, focus_tile_checks, index_calibration, index_field_tile_checks, index_replay_controls, noise_shrinkage_resource_controls, noise_shrinkage_weighted_references, preparation_self_check, prepare_case, prepare_real, replay_control_checks, replay_focus_baselines, run_crops, run_detector_controls, run_focus_configurations, run_focus_stack, run_focused_twins, run_baselines, run_equalized_context, run_index_configurations, run_index_full_references, run_index_stack, run_noise_shrinkage_configurations, run_noise_shrinkage_stack, run_selection_configurations, run_selection_replay, run_selection_stack, selection_streaming_resource_controls, selection_tile_self_checks, selection_width_allowances, selection_width_measurements


def _digest(value: np.ndarray) -> str:
    return hashlib.sha256(value.tobytes()).hexdigest()


def _render_cases(root: Path, seed: SeedTriple, allow_holdout: bool, prepare: object = prepare_case) -> list[str]:
    """Render every registered family member through one sealed seed boundary."""
    written = []
    for family in FAMILIES:
        for member in family_members(family):
            case, _data, _paths = prepare(root, family.name, member, seed, allow_holdout)
            written.append(str(case))
    return written


def _frozen_index_calibration(document: dict[str, object]) -> dict[str, object]:
    """Return calibration values frozen into the execution registration."""
    calibration = document.get("calibration")
    if not isinstance(calibration, dict) or calibration.get("state") != "measured":
        raise ValueError("registration does not contain frozen index-field calibration")
    thresholds = calibration.get("thresholds")
    if not isinstance(thresholds, dict) or set(thresholds) != {"1", "2", "4"}:
        raise ValueError("registration calibration thresholds are invalid")
    try:
        if (int(calibration["eligible_count"]) < 1 or not isinstance(calibration["raw_score_digest"], str)
                or len(calibration["raw_score_digest"]) != 64):
            raise ValueError
        normalized = {key: float(value) for key, value in thresholds.items()}
        if any(not math.isfinite(value) or value <= 0.0 for value in normalized.values()):
            raise ValueError
    except (KeyError, TypeError, ValueError) as error:
        raise ValueError("registration calibration values are invalid") from error
    return {**calibration, "thresholds": normalized}


def _validate_index_command(registration_path: Path, runs: Path, allowances: Path, source: str | None,
                            prepared: Path | None, rect: str | None, require_calibration: bool) -> dict[str, object]:
    """Reject incomplete field-experiment bindings before output creation."""
    if not registration_path.is_file() or not (runs / "baseline.json").is_file() or not allowances.is_file():
        raise ValueError("--registration, --runs baseline.json, and --allowances are required")
    try:
        document = json.loads(registration_path.read_text())
    except json.JSONDecodeError as error:
        raise ValueError("registration is not valid JSON") from error
    if document.get("arm") != "index-field" or document.get("execution_ready") is not True:
        raise ValueError("registration does not authorize index-field execution")
    package = Path(__file__).parent
    names = ("__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py", "focus_score.py", "index_field.py")
    current = {f"spikes/stacking_experiments/{name}": hashlib.sha256((package / name).read_bytes()).hexdigest() for name in names}
    if document.get("execution_module_sha256") != current:
        raise ValueError("registration does not bind the current index-field implementation")
    if hashlib.sha256((runs / "baseline.json").read_bytes()).hexdigest() != document.get("baseline_sha256"):
        raise ValueError("baseline records do not match the registration")
    focus_score._validate_baseline_inputs(runs / "baseline.json")
    if hashlib.sha256(allowances.read_bytes()).hexdigest() != document.get("allowances_sha256"):
        raise ValueError("allowances do not match the registration")
    if source is None:
        if prepared is not None or rect is not None:
            raise ValueError("--prepared and --rect require --source stack")
    elif prepared is None or not prepared.is_file() or rect not in (None, "1878,1056,1024,1024"):
        raise ValueError("--source stack requires the registered preparation manifest and rectangle")
    else:
        _validate_preparation(prepared, source)
        real = document.get("real_preparation")
        if not isinstance(real, dict) or real.get("manifest_sha256") != hashlib.sha256(prepared.read_bytes()).hexdigest():
            raise ValueError("preparation manifest does not match the registration")
    configurations, controls = document.get("configurations"), document.get("controls")
    if not isinstance(configurations, list) or not isinstance(controls, list):
        raise ValueError("registration configuration shape is invalid")
    expected_controls = {"C0", "C0r", "N1"}
    if {item.get("rung") for item in controls if isinstance(item, dict)} != expected_controls:
        raise ValueError("registration control ladder is invalid")
    if len(configurations) != 12 or any(not isinstance(item, dict) or item.get("rung") not in {"A1", "A2", "A3", "A4"}
                                      or item.get("threshold_multiplier") not in (1, 2, 4) or item.get("smoothing_radius") != 5
                                      for item in configurations):
        raise ValueError("registration candidate ladder is invalid")
    if require_calibration:
        _frozen_index_calibration(document)
    return document


def _frozen_selection_width_guards(document: dict[str, object]) -> dict[str, object]:
    """Return exactly the eight candidate guards fixed before candidate runs."""
    calibration = document.get("width_calibration")
    if not isinstance(calibration, dict) or calibration.get("state") != "measured":
        raise ValueError("registration does not contain frozen width guards")
    guards = calibration.get("allowances")
    if not isinstance(guards, list) or len(guards) != 8:
        raise ValueError("registration width guards are invalid")
    expected = {(width, tile, metric) for width in (1, 2) for tile in (512, 1024)
                for metric in ("hair_retention", "hair_label_exact")}
    actual = set()
    for guard in guards:
        if not isinstance(guard, dict):
            raise ValueError("registration width guard is invalid")
        key = (guard.get("width"), guard.get("tile"), guard.get("metric"))
        value = guard.get("allowance")
        if key in actual or key not in expected or guard.get("direction") != "higher" or not isinstance(value, (int, float)) or not math.isfinite(float(value)) or float(value) < 0.0:
            raise ValueError("registration width guard is invalid")
        seed_spread, repeat_spread = guard.get("seed_spread"), guard.get("repeat_spread")
        if not isinstance(seed_spread, (int, float)) or not isinstance(repeat_spread, (int, float)) or not math.isfinite(float(seed_spread)) or not math.isfinite(float(repeat_spread)) or float(value) != max(float(seed_spread), float(repeat_spread)):
            raise ValueError("registration width guard derivation is invalid")
        actual.add(key)
    if actual != expected:
        raise ValueError("registration width guard keys are incomplete")
    inputs = calibration.get("inputs")
    expected_inputs = {(seed.text(), tile, repeat) for seed in TRAINING_SEEDS for tile in (512, 1024) for repeat in (0, 1)}
    if not isinstance(inputs, list) or len(inputs) != 20:
        raise ValueError("registration width guard inputs are invalid")
    seen = set()
    for item in inputs:
        if not isinstance(item, dict):
            raise ValueError("registration width guard input is invalid")
        key = (item.get("seed"), item.get("tile"), item.get("repeat"))
        if key in seen or key not in expected_inputs or not isinstance(item.get("input_sha256"), dict) or not all(
                isinstance(item.get(name), str) and item[name] for name in ("composite_sha256", "zmap_sha256", "segment_mask_sha256", "support_mask_sha256", "normalization")):
            raise ValueError("registration width guard input is invalid")
        seen.add(key)
    if seen != expected_inputs:
        raise ValueError("registration width guard inputs are incomplete")
    return calibration


def _validate_selection_command(registration_path: Path, runs: Path, allowances: Path, source: str | None,
                                prepared: Path | None, rect: str | None, calibration_only: bool,
                                require_width_guards: bool) -> dict[str, object]:
    """Bind a consistency run to its frozen inputs before output creation."""
    if not registration_path.is_file() or not (runs / "baseline.json").is_file() or not allowances.is_file():
        raise ValueError("--registration, --runs baseline.json, and --allowances are required")
    try:
        document = json.loads(registration_path.read_text())
    except json.JSONDecodeError as error:
        raise ValueError("registration is not valid JSON") from error
    if document.get("arm") != "selection-consistency" or (not calibration_only and document.get("execution_ready") is not True):
        raise ValueError("registration does not authorize selection-consistency execution")
    package = Path(__file__).parent
    names = ("__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py", "focus_score.py", "index_field.py", "consistency.py")
    current = {f"spikes/stacking_experiments/{name}": hashlib.sha256((package / name).read_bytes()).hexdigest() for name in names}
    if document.get("execution_module_sha256") != current:
        raise ValueError("registration does not bind the current selection-consistency implementation")
    if hashlib.sha256((runs / "baseline.json").read_bytes()).hexdigest() != document.get("baseline_sha256"):
        raise ValueError("baseline records do not match the registration")
    focus_score._validate_baseline_inputs(runs / "baseline.json")
    if hashlib.sha256(allowances.read_bytes()).hexdigest() != document.get("allowances_sha256"):
        raise ValueError("allowances do not match the registration")
    candidates = document.get("configurations")
    controls = document.get("controls")
    expected_candidates = [
        ("consistency-majority-all", "majority-3x3-all", 192, 156),
        ("consistency-isolated-all", "isolated-all", 192, 156),
        ("consistency-cross-scale", "cross-scale-raw-parent", 192, 148),
        ("consistency-isolated-level0", "isolated-level0", 192, 140),
    ]
    if not isinstance(candidates, list) or len(candidates) != 4 or not all(isinstance(item, dict) for item in candidates) or [(item.get("id"), item.get("rule"), item.get("expanded_margin"), item.get("dependency_bound_pixels"))
                                            for item in candidates] != expected_candidates or any(
                                                item.get("classification") != "candidate" or item.get("can_advance") is not True
                                                for item in candidates):
        raise ValueError("registration candidate configuration is invalid")
    expected_controls = [
        ("consistency-identity", "identity", None, "control", 160, 140),
        ("consistency-destructive", "mode-9x9-all", None, "negative-control", 224, 204),
        ("consistency-random-majority-all", "matched-random", "consistency-majority-all", "control", None, "global; no local cone claim"),
        ("consistency-random-isolated-all", "matched-random", "consistency-isolated-all", "control", None, "global; no local cone claim"),
        ("consistency-random-cross-scale", "matched-random", "consistency-cross-scale", "control", None, "global; no local cone claim"),
        ("consistency-random-isolated-level0", "matched-random", "consistency-isolated-level0", "control", None, "global; no local cone claim"),
    ]
    if not isinstance(controls, list) or len(controls) != 6 or not all(isinstance(item, dict) for item in controls) or [(item.get("id"), item.get("rule"), item.get("matched_candidate"), item.get("classification"),
                                           item.get("expanded_margin"), item.get("dependency_bound_pixels"))
                                          for item in controls] != expected_controls or any(
                                              item.get("can_advance") is not False for item in controls if isinstance(item, dict)):
        raise ValueError("registration control configuration is invalid")
    random_control = document.get("random_control")
    if not isinstance(random_control, dict) or random_control.get("bit_generator") != "PCG64" or random_control.get("position_order") != "immutable global level coordinates, C-order row-major" or random_control.get("tile_in_seed") is not False or random_control.get("seed_rule") != "uint64 little-endian first8bytes SHA256 of UTF8 consistency-random-v1|family|member|seedtext|candidate_id|level|purpose" or random_control.get("purposes") != ["sources", "positions"] or random_control.get("independent_from") != "scene layout/texture/noise RNG streams":
        raise ValueError("registration random-control configuration is invalid")
    if document.get("levels") != 5 or document.get("score_radii") != [8, 4, 2, 1, 1] or document.get("current_tile_margin") != 160 or document.get("tiles") != [512, 1024] or document.get("equalize") is not False or document.get("candidate_count") != 4 or document.get("control_rule_count") != 3 or document.get("configuration_cap") != 7 or document.get("output_configuration_count") != 10:
        raise ValueError("registration fixed selection controls are invalid")
    if source is None:
        if prepared is not None or rect is not None:
            raise ValueError("--prepared and --rect require --source stack")
    elif prepared is None or rect not in (None, "1878,1056,1024,1024"):
        raise ValueError("--source stack requires the registered preparation manifest and rectangle")
    else:
        manifest = _validate_preparation(prepared, source)
        real = document.get("real_preparation")
        if not isinstance(real, dict) or real.get("manifest_sha256") != hashlib.sha256(prepared.read_bytes()).hexdigest() or real.get("report_sha256") != manifest.get("alignment_report_sha256"):
            raise ValueError("preparation does not match the registration")
    if require_width_guards:
        _frozen_selection_width_guards(document)
        artifact = document.get("width_calibration_artifact")
        if not isinstance(artifact, dict) or artifact.get("path") != ".scratch/selection-consistency/calibration/width-calibration.json" or not isinstance(artifact.get("sha256"), str):
            raise ValueError("registration width guard artifact is invalid")
        artifact_path = Path(str(artifact["path"]))
        if not artifact_path.is_file() or hashlib.sha256(artifact_path.read_bytes()).hexdigest() != artifact["sha256"]:
            raise ValueError("registration width guard artifact is missing or stale")
        try:
            if json.loads(artifact_path.read_text()) != document["width_calibration"]:
                raise ValueError("registration width guard artifact does not match")
        except json.JSONDecodeError as error:
            raise ValueError("registration width guard artifact is invalid JSON") from error
    document["_validated_registration_sha256"] = hashlib.sha256(registration_path.read_bytes()).hexdigest()
    return document


def _validate_noise_shrinkage_command(registration_path: Path, runs: Path, allowances: Path,
                                      source: str | None, prepared: Path | None, rect: str | None) -> dict[str, object]:
    """Reject unbound shrinkage execution before an output directory exists."""
    if not registration_path.is_file() or not (runs / "baseline.json").is_file() or not allowances.is_file():
        raise ValueError("--registration, --runs baseline.json, and --allowances are required")
    try:
        document = json.loads(registration_path.read_text())
    except json.JSONDecodeError as error:
        raise ValueError("registration is not valid JSON") from error
    if document.get("arm") != "noise-shrinkage" or document.get("execution_ready") is not True:
        raise ValueError("registration does not authorize noise-shrinkage execution")
    package = Path(__file__).parent
    names = ("__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py", "focus_score.py", "index_field.py", "consistency.py", "coefficient_shrinkage.py")
    current = {f"spikes/stacking_experiments/{name}": hashlib.sha256((package / name).read_bytes()).hexdigest() for name in names}
    if document.get("execution_module_sha256") != current:
        raise ValueError("registration does not bind the current noise-shrinkage implementation")
    if hashlib.sha256((runs / "baseline.json").read_bytes()).hexdigest() != document.get("baseline_sha256"):
        raise ValueError("baseline records do not match the registration")
    focus_score._validate_baseline_inputs(runs / "baseline.json")
    if hashlib.sha256(allowances.read_bytes()).hexdigest() != document.get("allowances_sha256"):
        raise ValueError("allowances do not match the registration")
    expected_threshold = {"energy": "sum_c coefficient_c^2 in f64 squared units", "estimator": "per frame and level: MAD of covered finite coefficient components / 0.6745; median across frames; threshold is multiplier times squared estimate", "exclude": ["NO_SOURCE", "outside coverage", "non-finite coefficient"], "empty_support": "unavailable", "zero_multiplier": "explicit exact f32 bypass"}
    expected_controls = {"baseline": "unshrunk selected details", "gaussian_blur": {"strengths": [0.25, 0.5, 0.75, 1.0, 1.5, 2.0]}, "uniform_detail_attenuation": {"strengths": [0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875]}, "match_metric": "flat_noise from truth-residual luma standard deviation", "selection": "evaluate every strength; choose least absolute error; listed order breaks ties", "failure": "unavailable metric, missing region, empty support or unmatched control forces Defer"}
    expected_seeds = ["101,201,301", "102,202,302", "103,203,303", "104,204,304", "105,205,305"]
    expected_matrix = {"candidate_multipliers": 4, "controls_per_candidate": 2, "identity": 1, "reference": "Weighted Average", "quality_output": "blocked until final exact source binding", "training_record_slots": 780, "training_input_keys": 60, "flat_repeat_record_slots": 260, "flat_repeat_keys": 20, "weighted_references_training": 60, "weighted_references_repeat": 20, "real_record_slots": 13, "real_montages": 4, "unmatched": "record retained with explicit reasons; absent rendering is not an output image"}
    expected_matching = {"metric": "flat_noise", "source": "unchanged frozen allowances", "key": ["family", "member", "laplacian", "tile", "flat_noise"], "accept": "absolute error <= keyed allowance", "missing_key": "unmatched; Defer"}
    expected_noise_free = {"kind": "constructed constant two-frame RGB; zero injected noise", "shape_hwc": [193, 197, 3], "sample_value": 32768, "coverage": ["all-valid", "two-pixel all-frame border invalid"], "multipliers": [0, 1, 2, 4, 8], "result_checks": 10, "criterion": "decoded composite and Zmap exactly equal threshold-zero reference, stronger than any nonnegative tolerance", "limit": "constant-input boundary proof; not a new canonical quality family or an arbitrary noise-free structure safety claim"}
    if (document.get("method") != "laplacian" or document.get("multipliers") != [0, 1, 2, 4, 8] or document.get("levels") != 5 or document.get("score_radii") != [8, 4, 2, 1, 1]
            or document.get("equalize") is not False or document.get("threshold") != expected_threshold or document.get("controls") != expected_controls
            or document.get("training") != {"seeds": expected_seeds, "tiles": [512, 1024], "holdout": "sealed"}
            or document.get("matching_tolerance") != expected_matching or document.get("output_matrix") != expected_matrix
            or document.get("controlled_noise_free") != expected_noise_free):
        raise ValueError("registration shrinkage ladder is invalid")
    if source is None:
        if prepared is not None or rect is not None:
            raise ValueError("--prepared and --rect require --source stack")
    else:
        if prepared is None or rect not in (None, "1878,1056,1024,1024"):
            raise ValueError("--source stack requires the registered preparation manifest and rectangle")
        _validate_preparation(prepared, source)
        real = document.get("real")
        expected_diagnostic = {"luma": "arithmetic mean of RGB channels in uint16 sample units", "statistic": "MAD(Y - blur(Y, sigma=1.0)) / 0.6745 over covered ROI", "search": "same fixed control strength grids; minimum absolute error; listed order breaks ties", "status": "diagnostic-only", "guard": "no synthetic truth, canonical allowance or numeric matched-pass claim", "missing_support": "unavailable; Defer"}
        if not isinstance(real, dict) or real.get("prepared_manifest_sha256") != hashlib.sha256(prepared.read_bytes()).hexdigest() or real.get("alignment_report_sha256") != _validate_preparation(prepared, source).get("alignment_report_sha256") or real.get("rect_xywh") != [1878, 1056, 1024, 1024] or real.get("diagnostic") != expected_diagnostic:
            raise ValueError("preparation manifest does not match the registration")
    document["_validated_registration_sha256"] = hashlib.sha256(registration_path.read_bytes()).hexdigest()
    return document


def self_check(allowances: Path | None = None) -> int:
    validate_registration()
    preparation_self_check()
    if baseline_call_count(TRAINING_SEEDS) != 360:
        raise AssertionError("registered baseline call count is not 360")
    require_seed(TRAINING_SEEDS[0])
    try:
        require_seed(HOLDOUT_SEEDS[0])
    except ValueError as error:
        if "sealed hold-out split" not in str(error):
            raise
    else:
        raise AssertionError("sealed seed was accepted")
    if case_split(HOLDOUT_SEEDS[0], True) != "holdout":
        raise AssertionError("hold-out override did not reach the case boundary")
    try:
        case_split(SeedTriple(1, 2, 3))
    except ValueError:
        pass
    else:
        raise AssertionError("unknown case seed was accepted")
    received = []
    def prepared(root: Path, family: str, member: str, seed: SeedTriple, allow_holdout: bool) -> tuple[Path, None, list[Path]]:
        received.append(allow_holdout)
        return root / family / member, None, []
    if len(_render_cases(Path("unused"), HOLDOUT_SEEDS[0], True, prepared)) != 11 or not all(received):
        raise AssertionError("hold-out override did not reach case preparation")
    source = np.arange(9 * 11 * 3, dtype=np.float32).reshape(9, 11, 3)
    reduced = fusion.reduce(source)
    restored = fusion.expand(reduced, 9, 11)
    if reduced.shape != (5, 6, 3) or restored.shape != source.shape:
        raise AssertionError("odd-size pyramid shape mismatch")
    pyramid = fusion.pyramid_views(np.stack((source, source + 3.0)), 3, coverage=np.ones((2, 9, 11), dtype=bool))
    if [level.gaussian.shape[1:3] for level in pyramid.levels] != [(9, 11), (5, 6), (3, 3)] or pyramid.residual_inputs.shape[1:3] != (2, 2):
        raise AssertionError("per-level odd-size pyramid shape mismatch")
    if [level.score_radius for level in pyramid.levels] != [8, 4, 2] or [level.reach for level in pyramid.levels] != [10, 14, 22] or pyramid.residual_reach != 14:
        raise AssertionError("pyramid reach mismatch")
    if not np.array_equal(fusion.ascending_f32_mean(np.stack((source, source + 2.0))), source + 1.0):
        raise AssertionError("ascending f32 mean mismatch")
    family = FAMILIES[0]
    rendered = render_family(family, TRAINING_SEEDS[0])
    repeated = render_family(family, TRAINING_SEEDS[0])
    texture_changed = render_family(family, SeedTriple(101, 202, 301))
    noise_changed = render_family(family, SeedTriple(101, 201, 302))
    if not np.array_equal(rendered.frames, repeated.frames) or not np.array_equal(rendered.truth, repeated.truth):
        raise AssertionError("same seed triple is not deterministic")
    if np.array_equal(rendered.truth, texture_changed.truth) or not np.array_equal(rendered.depth, texture_changed.depth):
        raise AssertionError("texture seed changed the wrong scene component")
    if np.array_equal(rendered.frames, noise_changed.frames) or not np.array_equal(rendered.truth, noise_changed.truth):
        raise AssertionError("noise seed changed the wrong scene component")
    colour = render_family(FAMILIES[2], TRAINING_SEEDS[0])
    y0, y1, x0, x1 = colour.metadata["step"]
    boundary = colour.metadata["colour_boundary_y"]
    if not y0 < boundary < y1 or not all(colour.depth[boundary, x - 1] != colour.depth[boundary, x] for x in (x0, x1)):
        raise AssertionError("colour boundary does not cross both depth-step sides")
    channel = render_family(FAMILIES[5], TRAINING_SEEDS[0], "channel")
    lighting = channel.metadata["lighting"]
    if not any(any(value != 0 for value in row) for row in lighting["channel_offset_counts"]):
        raise AssertionError("channel offsets are absent")
    if len(lighting["clip_fraction"]) != 8:
        raise AssertionError("lighting clipping record is incomplete")
    frames = np.stack((source, source + 3.0))
    coverage = np.ones((2, 9, 11), dtype=bool)
    coverage[:, 4, 5] = False
    covered_pyramid = fusion.pyramid_views(frames, 2, coverage=coverage)
    if covered_pyramid.levels[1].raw_coverage[:, 2, 2].any() or covered_pyramid.levels[1].eligible.any():
        raise AssertionError("per-level eligibility did not use original-mask reach")
    details = fusion.details_level0(frames, coverage)
    depth = fusion.depth_map(frames, coverage)
    if details.winners[4, 5] != fusion.NO_SOURCE or depth.winners[4, 5] != fusion.NO_SOURCE:
        raise AssertionError("coverage was not eroded")
    tie = fusion.select(np.ones((2, 2, 2), dtype=np.float32), np.ones((2, 2, 2), dtype=bool))
    if not np.array_equal(tie, np.zeros((2, 2), dtype=np.uint16)):
        raise AssertionError("ties must keep first frame")
    try:
        _twin_disagreements(np.array([[0]], dtype=np.uint16), np.array([[1]], dtype=np.uint16),
                             np.array([[[0.0]], [[1.0]]], dtype=np.float32), np.ones((2, 1, 1), dtype=bool), 0.5)
    except AssertionError:
        pass
    else:
        raise AssertionError("outside-tolerance twin disagreement was accepted")
    print("seed split: ok")
    print("preparation validation: ok")
    print(f"frames sha256: {_digest(rendered.frames)}")
    print(f"truth sha256: {_digest(rendered.truth)}")
    print(f"pyramid sha256: {_digest(restored)}")
    print(f"details winners sha256: {_digest(details.winners)}")
    print(f"depth winners sha256: {_digest(depth.winners)}")
    for name in focus_score.self_checks():
        print(f"focus score {name}: ok")
    for check in focus_score.radius10_support_checks() + focus_tile_checks():
        print(f"focus score {check['name']}: ok {json.dumps(check, sort_keys=True)}")
    for name in replay_control_checks():
        print(f"focus score {name}: ok")
    for name in focus_configuration_checks():
        print(f"focus score {name}: ok")
    for name in index_field.self_checks():
        print(f"{name}: ok")
    for name in consistency.self_checks():
        print(f"{name}: ok")
    for record in selection_tile_self_checks():
        print(f"{record['name']}: ok {json.dumps(record, sort_keys=True)}")
    if allowances is not None:
        for record in selection_streaming_resource_controls(Path(".scratch/stacking-controls")):
            print(f"streaming resource N={record['frame_count']}: reads={record['sample_reads']} rss_kb={record['peak_rss_kb']}")
    for check in index_field_tile_checks():
        print(f"{check['name']}: ok {json.dumps(check, sort_keys=True)}")
    if allowances is None:
        print("detectors: allowances required")
    else:
        controls = run_detector_controls(allowances)
        for control in controls:
            print(f"detector {control['defect']} x{control['factor']}: {control['status']}")
        print("detector clean: ok")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-check", action="store_true")
    parser.add_argument("--allowances")
    sub = parser.add_subparsers(dest="command")
    cases = sub.add_parser("cases")
    cases.add_argument("--seed", required=True)
    cases.add_argument("--allow-holdout", action="store_true")
    cases.add_argument("--out", required=True)
    baseline = sub.add_parser("baseline")
    baseline.add_argument("--split", choices=("training",), required=True)
    baseline.add_argument("--out", required=True)
    detectors = sub.add_parser("detectors")
    detectors.add_argument("--allowances", required=True)
    detectors.add_argument("--out", required=True)
    twins = sub.add_parser("twins")
    twins.add_argument("--runs", required=True)
    twins.add_argument("--out", required=True)
    crops = sub.add_parser("crops")
    crops.add_argument("--source", choices=("stack",), required=True)
    crops.add_argument("--prepared")
    crops.add_argument("--rect")
    crops.add_argument("--out", required=True)
    prepare_real_command = sub.add_parser("prepare-real")
    prepare_real_command.add_argument("--source", choices=("stack",), required=True)
    prepare_real_command.add_argument("--out", required=True)
    registration_command = sub.add_parser("registration")
    registration_command.add_argument("--out")
    focus = sub.add_parser("focus-score")
    focus.add_argument("--split", choices=("training",), default="training")
    focus.add_argument("--registration", required=True)
    focus.add_argument("--runs", required=True)
    focus.add_argument("--allowances", required=True)
    focus.add_argument("--out", required=True)
    focus.add_argument("--replay-only", action="store_true")
    focus.add_argument("--source", choices=("stack",))
    focus.add_argument("--prepared")
    focus.add_argument("--rect")
    index = sub.add_parser("index-field")
    index.add_argument("--split", choices=("training",), default="training")
    index.add_argument("--registration", required=True)
    index.add_argument("--runs", required=True)
    index.add_argument("--allowances", required=True)
    index.add_argument("--out", required=True)
    index.add_argument("--family")
    index.add_argument("--replay-only", action="store_true")
    index.add_argument("--calibration-only", action="store_true")
    index.add_argument("--source", choices=("stack",))
    index.add_argument("--prepared")
    index.add_argument("--rect")
    selection = sub.add_parser("selection-consistency")
    selection.add_argument("--split", choices=("training",), default="training")
    selection.add_argument("--registration", required=True)
    selection.add_argument("--runs", required=True)
    selection.add_argument("--allowances", required=True)
    selection.add_argument("--out", required=True)
    selection.add_argument("--family")
    selection.add_argument("--replay-only", action="store_true")
    selection.add_argument("--calibration-only", action="store_true")
    selection.add_argument("--source", choices=("stack",))
    selection.add_argument("--prepared")
    selection.add_argument("--rect")
    shrinkage = sub.add_parser("noise-shrinkage")
    shrinkage.add_argument("--self-check", dest="shrinkage_self_check", action="store_true")
    shrinkage.add_argument("--split", choices=("training",), default="training")
    shrinkage.add_argument("--registration")
    shrinkage.add_argument("--runs")
    shrinkage.add_argument("--allowances")
    shrinkage.add_argument("--out")
    shrinkage.add_argument("--source", choices=("stack",))
    shrinkage.add_argument("--prepared")
    shrinkage.add_argument("--rect")
    shrinkage.add_argument("--repeat-flat", action="store_true")
    args = parser.parse_args(argv)
    if args.self_check:
        return self_check(Path(args.allowances) if args.allowances else None)
    if args.command == "registration":
        document = json.dumps(registration(), indent=2, sort_keys=True)
        if args.out:
            Path(args.out).write_text(document + "\n")
        print(document)
        return 0
    if args.command == "noise-shrinkage":
        if args.shrinkage_self_check:
            for name in coefficient_shrinkage.self_checks():
                print(f"{name}: ok")
            return 0
        if not all((args.registration, args.runs, args.allowances, args.out)):
            parser.error("--registration, --runs, --allowances, and --out are required")
        if args.source == "stack" and not os.environ.get("FOCUSFORGE_REFERENCE_STACK"):
            print("FOCUSFORGE_REFERENCE_STACK: real work skipped")
            return 0
        try:
            document = _validate_noise_shrinkage_command(Path(args.registration), Path(args.runs), Path(args.allowances), args.source,
                                                         Path(args.prepared) if args.prepared else None, args.rect)
        except ValueError as error:
            parser.error(str(error))
        output = Path(args.out)
        output.mkdir(parents=True, exist_ok=True)
        started = time.monotonic()
        stack = run_noise_shrinkage_stack(output, args.source, Path(args.prepared), args.rect) if args.source == "stack" else None
        family = "flat-noise" if args.repeat_flat else None
        replay = run_selection_replay(Path(args.runs)) if stack is None and not args.repeat_flat else []
        rows = stack["rows"] if stack is not None else run_noise_shrinkage_configurations(Path(args.runs), output, Path(args.allowances), family_filter=family)
        references = [] if stack is not None else noise_shrinkage_weighted_references(Path(args.runs), family)
        resources = [] if stack is not None or args.repeat_flat else noise_shrinkage_resource_controls(Path(args.runs))
        (output / "comparison.json").write_text(json.dumps({"command": sys.argv if argv is None else [sys.argv[0], *argv],
            "registration_sha256": document["_validated_registration_sha256"], "replay": replay, "candidate_rows": rows, "weighted_references": references, "resource_controls": resources, "stack": stack,
            "budget": {"measured_seconds": time.monotonic() - started}}, indent=2, sort_keys=True) + "\n")
        print(f"candidate rows: {len(rows)}")
        return 0
    if args.command == "focus-score":
        command_started = time.monotonic()
        try:
            document = focus_score.validate_command(Path(args.registration), Path(args.runs), Path(args.allowances),
                                                    args.source, Path(args.prepared) if args.prepared else None, args.rect)
        except ValueError as error:
            parser.error(str(error))
        budget = {"session_cap_seconds": 7200, "total_cap_seconds": 21600, "total_enforcement": "external aggregate",
                  "started_monotonic": command_started, "exhausted": False}
        records = replay_focus_baselines(Path(args.runs))
        output = Path(args.out)
        output.mkdir(parents=True, exist_ok=True)
        configurations = document["matched_controls"] + document["configurations"]
        if args.replay_only:
            candidates, stack = [], None
        elif args.source == "stack":
            stack = run_focus_stack(output, args.source, Path(args.prepared), args.rect, configurations, budget)
            candidates = stack["rows"]
        else:
            candidates, stack = run_focus_configurations(Path(args.runs), output, configurations, Path(args.allowances), budget), None
        budget["measured_seconds"] = time.monotonic() - budget.pop("started_monotonic")
        support = focus_support_records(candidates)
        (output / "comparison.json").write_text(json.dumps({"command": sys.argv if argv is None else [sys.argv[0], *argv], "registration_sha256": document["_validated_registration_sha256"], "replay": records, "candidate_rows": candidates, "support": support,
                                                               "support_controls": focus_score.radius10_support_checks(), "stack": stack,
                                                               "budget": budget}, indent=2, sort_keys=True) + "\n")
        print(f"replay records: {len(records)}")
        return 0
    if args.command == "index-field":
        registration_path, runs, allowances = Path(args.registration), Path(args.runs), Path(args.allowances)
        try:
            document = _validate_index_command(registration_path, runs, allowances, args.source,
                                               Path(args.prepared) if args.prepared else None, args.rect,
                                               not args.replay_only and not args.calibration_only)
        except ValueError as error:
            parser.error(str(error))
        if args.family and args.family not in {family.name for family in FAMILIES if family.fusion_baseline}:
            parser.error("--family must name a registered fusion family")
        if args.calibration_only and document.get("calibration", {}).get("state") == "measured":
            parser.error("registration already contains frozen index-field calibration")
        started = time.monotonic()
        budget = {"session_cap_seconds": 7200, "total_cap_seconds": 21600, "total_enforcement": "external aggregate",
                  "started_monotonic": started, "exhausted": False}
        output = Path(args.out)
        if args.calibration_only:
            output.mkdir(parents=True, exist_ok=True)
            calibration = index_calibration(runs)
            (output / "calibration.json").write_text(json.dumps(calibration, indent=2, sort_keys=True) + "\n")
            replay, candidates, stack, full_references = [], [], None, []
        else:
            replay = index_replay_controls(runs)
            output.mkdir(parents=True, exist_ok=True)
            configurations = document["controls"] + document["configurations"]
            calibration = _frozen_index_calibration(document) if not args.replay_only else None
        if args.replay_only:
            calibration, candidates, stack, full_references = None, [], None, []
        elif args.calibration_only:
            pass
        elif args.source == "stack":
            stack = run_index_stack(output, args.source, Path(args.prepared), args.rect, configurations, calibration)
            candidates, full_references = stack["rows"], []
        else:
            candidates = run_index_configurations(runs, output, configurations, allowances, calibration, budget, args.family)
            stack = None
            full_references = [] if args.family else run_index_full_references(runs, output, configurations, calibration, budget)
        budget["measured_seconds"] = time.monotonic() - started
        (output / "comparison.json").write_text(json.dumps({"command": sys.argv if argv is None else [sys.argv[0], *argv],
            "registration_sha256": hashlib.sha256(registration_path.read_bytes()).hexdigest(), "replay": replay,
            "calibration": calibration, "candidate_rows": candidates, "full_reference_rows": full_references,
            "stack": stack, "budget": budget}, indent=2, sort_keys=True) + "\n")
        print(f"replay records: {len(replay)}")
        return 0
    if args.command == "selection-consistency":
        try:
            document = _validate_selection_command(Path(args.registration), Path(args.runs), Path(args.allowances), args.source,
                                                   Path(args.prepared) if args.prepared else None, args.rect, args.calibration_only,
                                                   not args.calibration_only and not args.replay_only)
        except ValueError as error:
            parser.error(str(error))
        if args.family and args.family != "flat-noise":
            parser.error("--family supports flat-noise only for the registered repeat")
        if args.calibration_only:
            if document.get("width_calibration", {}).get("state") == "measured":
                parser.error("registration already contains frozen width guards")
            output = Path(args.out)
            output.mkdir(parents=True, exist_ok=True)
            calibration = selection_width_allowances(selection_width_measurements(Path(args.runs)))
            (output / "width-calibration.json").write_text(json.dumps(calibration, indent=2, sort_keys=True) + "\n")
            print("width guard rows: 8")
            return 0
        output = Path(args.out)
        output.mkdir(parents=True, exist_ok=True)
        started = time.monotonic()
        budget = {"session_cap_seconds": 7200, "total_cap_seconds": 21600, "total_enforcement": "external aggregate",
                  "started_monotonic": started, "exhausted": False}
        replay = run_selection_replay(Path(args.runs), budget) if args.family is None and args.source is None else []
        if args.replay_only:
            candidates, stack = [], None
        elif args.source == "stack":
            if time.monotonic() - started >= 7200:
                budget["exhausted"] = True
                budget["deferred"] = ["real crop"]
                candidates, stack = [], None
            else:
                candidates, stack = [], run_selection_stack(output, args.source, Path(args.prepared), args.rect,
                                                             document["configurations"], document["controls"])
        else:
            candidates = run_selection_configurations(Path(args.runs), output, document["configurations"], document["controls"],
                                                      Path(args.allowances), args.family, budget)
            stack = None
        budget["measured_seconds"] = time.monotonic() - started
        (output / "comparison.json").write_text(json.dumps({"command": sys.argv if argv is None else [sys.argv[0], *argv],
            "registration_sha256": document["_validated_registration_sha256"], "replay": replay,
            "width_guards": document["width_calibration"], "candidate_rows": candidates, "stack": stack,
            "budget": budget}, indent=2, sort_keys=True) + "\n")
        print(f"replay records: {len(replay)}")
        print(f"candidate rows: {len(candidates)}")
        return 0
    if args.command == "baseline":
        root = Path(args.out)
        rows = run_baselines(root, TRAINING_SEEDS)
        context = run_equalized_context(root, TRAINING_SEEDS[0])
        print(f"baseline records: {len(rows)}")
        print(f"equalized context records: {len(context)}")
        return 0
    if args.command == "detectors":
        controls = run_detector_controls(Path(args.allowances), Path(args.out))
        for control in controls:
            print(f"detector {control['defect']} x{control['factor']}: {control['status']}")
        print("detector clean: ok")
        if any(control["status"] != "ok" for control in controls):
            return 1
        print(f"detector controls: {len(controls)}")
        return 0
    if args.command == "twins":
        checks = run_focused_twins(Path(args.runs))
        Path(args.out).write_text(json.dumps(checks, indent=2, sort_keys=True) + "\n")
        for check in checks:
            margins = [item["margin_f32"] for item in check["disagreements"]]
            print(f"twin {check['family']}/{check['member']} {check['method']}: disagreements={check['cli_twin_disagreements']} max_score_margin={max(margins, default=0.0)}")
        print(f"focused twin checks: {len(checks)}")
        return 0
    if args.command == "cases":
        try:
            split = case_split(SeedTriple.parse(args.seed), args.allow_holdout)
        except ValueError as error:
            parser.error(str(error))
        root = Path(args.out)
        written = _render_cases(root, SeedTriple.parse(args.seed), args.allow_holdout)
        (root / "cases.json").write_text(json.dumps({"split": split, "seed": args.seed, "cases": written}, indent=2) + "\n")
        print(f"{split} cases: {len(written)}")
        return 0
    if args.command == "crops":
        try:
            rows = run_crops(Path(args.out), args.source, Path(args.prepared) if args.prepared else None, args.rect)
        except ValueError as error:
            parser.error(str(error))
        print(f"crop records: {len(rows)}")
        return 0
    if args.command == "prepare-real":
        try:
            manifest = prepare_real(Path(args.out), args.source)
        except ValueError as error:
            parser.error(str(error))
        print(f"preparation manifest: {manifest}")
        return 0
    parser.print_help()
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
