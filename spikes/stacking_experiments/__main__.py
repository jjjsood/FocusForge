"""Command interface for fixed stack-experiment controls."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys

import numpy as np

from . import fusion
from .protocol import FAMILIES, HOLDOUT_SEEDS, TRAINING_SEEDS, SeedTriple, registration, require_seed, validate_registration
from .scenes import family_members, render_family
from .runner import _twin_disagreements, baseline_call_count, case_split, preparation_self_check, prepare_case, prepare_real, run_crops, run_detector_controls, run_focused_twins, run_baselines, run_equalized_context


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
    args = parser.parse_args(argv)
    if args.self_check:
        return self_check(Path(args.allowances) if args.allowances else None)
    if args.command == "registration":
        document = json.dumps(registration(), indent=2, sort_keys=True)
        if args.out:
            Path(args.out).write_text(document + "\n")
        print(document)
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
