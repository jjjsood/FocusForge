"""CPU baseline records and reproducible input preparation."""

from __future__ import annotations

from collections import defaultdict
from dataclasses import asdict, is_dataclass
from datetime import datetime, timezone
from hashlib import sha256
import json
import math
import os
from pathlib import Path
import platform
import resource
import shutil
import subprocess
import sys
import time
from typing import Iterable

import numpy as np
import PIL

from . import consistency, fusion, index_field, metrics
from .protocol import FAMILIES, FRAME_COUNT, REAL_CROPS, TRAINING_SEEDS, SeedTriple, require_seed
from .scenes import SceneData, family_members, render_family

ROOT = Path(__file__).resolve().parents[2]
GFF_DIR = ROOT / "spikes" / "guided-filter-fusion"
SYNTH_DIR = ROOT / "spikes" / "synthetic-ground-truth"
for directory in (GFF_DIR, SYNTH_DIR):
    if str(directory) not in sys.path:
        sys.path.insert(0, str(directory))
import gff_fusion as gff  # noqa: E402

METHODS = ("laplacian", "weighted-average", "depth-map")
TILES = (512, 1024)
REPEATS = (0, 1)


def sha256_path(path: Path) -> str:
    """Hash one file without making its parent part of the record."""
    digest = sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def provenance(inputs: Iterable[Path]) -> dict[str, object]:
    """Capture stable local executable and interpreter identifiers."""
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    cli = ROOT / "target" / "debug" / "focusforge-cli"
    modules = ("__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py", "focus_score.py", "index_field.py", "consistency.py")
    cpu_model = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), platform.processor())
    return {"revision": revision, "cli_sha256": sha256_path(cli) if cli.exists() else None,
            "input_sha256": {path.name: sha256_path(path) for path in inputs},
            "runtime": {"python": sys.version.split()[0], "platform": platform.platform(),
                        "numpy": np.__version__, "pillow": PIL.__version__, "cpu_only": True,
                        "cpu_model": cpu_model, "logical_cpu_count": os.cpu_count()},
            "module_sha256": {name: sha256_path(Path(__file__).with_name(name)) for name in modules},
            "vram": "not used"}


def run_key(family: str, seed: SeedTriple, method: str, tile: int, repeat: int) -> str:
    """Return a unique output key for repeated independent runs."""
    return f"{family}-{seed.layout}-{seed.texture}-{seed.noise}-{method}-t{tile}-r{repeat}"


def _json(value: object) -> object:
    if is_dataclass(value):
        return _json(asdict(value))
    if isinstance(value, np.ndarray):
        return value.tolist()
    if isinstance(value, np.integer):
        return int(value)
    if isinstance(value, np.floating):
        return float(value)
    if isinstance(value, slice):
        return [value.start, value.stop]
    if isinstance(value, tuple):
        return [_json(item) for item in value]
    if isinstance(value, list):
        return [_json(item) for item in value]
    if isinstance(value, dict):
        return {str(key): _json(item) for key, item in value.items()}
    return value


def _write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(_json(value), indent=2, sort_keys=True, allow_nan=False) + "\n")


def _case_dir(root: Path, family: str, member: str, seed: SeedTriple) -> Path:
    return root / "cases" / f"{family}-{member}-l{seed.layout}-t{seed.texture}-n{seed.noise}"


def case_split(seed: SeedTriple, allow_holdout: bool = False) -> str:
    """Validate one case seed before any scene data is rendered."""
    return require_seed(seed, allow_holdout)


def prepare_case(root: Path, family_name: str, member: str, seed: SeedTriple, allow_holdout: bool = False) -> tuple[Path, SceneData, list[Path]]:
    """Write one deterministic registered input and return its in-memory truth."""
    case_split(seed, allow_holdout)
    family = next(item for item in FAMILIES if item.name == family_name)
    data = render_family(family, seed, member)
    case = _case_dir(root, family_name, member, seed)
    frames = case / "frames"
    frames.mkdir(parents=True, exist_ok=True)
    paths = []
    for index, frame in enumerate(data.frames):
        path = frames / f"{index:02d}.tif"
        gff.write_u16(path, frame)
        paths.append(path)
    gff.write_u16(case / "reference.tif", data.truth)
    gff.write_u16(case / "truth-zmap.tif", np.rint(data.depth)[..., None])
    if "coverage" in data.metadata:
        np.save(case / "masks.npy", data.metadata["coverage"])
    _write_json(case / "case.json", {"family": family_name, "member": member, "seed": seed, "metadata": data.metadata,
                                       "frame_sha256": [sha256_path(path) for path in paths], "truth_sha256": sha256_path(case / "reference.tif")})
    return case, data, paths


def _source(case: Path) -> object:
    masks = case / "masks.npy"
    return gff.TiffSource(gff.frame_paths(case), masks if masks.exists() else None)


def _axis_boundaries(extent: int, interior: int, margin: int) -> tuple[int, ...]:
    parts, position = [], 0
    while position < extent:
        size = min(interior, extent - position)
        parts.append([position, size])
        position += size
    if len(parts) > 1 and parts[-1][1] < margin:
        parts[-2][1] += parts.pop()[1]
    return tuple(part[0] for part in parts[1:])


def tile_boundaries(width: int, height: int, tile: int, margin: int, levels: int) -> dict[str, object]:
    """Reproduce the scheduler's interior boundaries from its reported support."""
    alignment = 1 << levels
    interior = ((tile - 2 * margin) // alignment) * alignment
    if interior <= 0:
        raise ValueError("reported tile support has no interior")
    return {"margin": margin, "levels": levels, "interior": interior,
            "vertical": _axis_boundaries(width, interior, margin), "horizontal": _axis_boundaries(height, interior, margin)}


def tile_read_bound(width: int, height: int, plan: dict[str, object]) -> dict[str, int]:
    """Return the largest halo-expanded read implied by a tile plan."""
    margin = int(plan["margin"])
    def maximum(length: int, boundaries: list[object]) -> int:
        edges = _tile_edges(length, boundaries)
        return max(min(length, end + margin) - max(0, start - margin) for start, end in zip(edges, edges[1:]))
    return {"width": maximum(width, list(plan["vertical"])), "height": maximum(height, list(plan["horizontal"]))}


def _case_metrics(comp: Path, zmap: Path, data: SceneData, source: object, boundaries: dict[str, object]) -> dict[str, float | None]:
    image = gff.read_u16(comp)
    result = metrics.measure(image, data.truth, data.metadata, source, tuple(boundaries["vertical"]), tuple(boundaries["horizontal"]))
    z = gff.read_zmap(zmap)
    coverage = data.metadata.get("coverage")
    valid = coverage.any(axis=0) if isinstance(coverage, np.ndarray) else np.ones(z.shape, dtype=bool)
    valid &= z != 65535
    result.update(metrics.label_metrics(z, np.rint(data.depth).astype(np.uint16), valid))
    return result


def _sets(method: str, equalized: bool) -> tuple[str, ...]:
    sets = [f"equalize={'true' if equalized else 'false'}"]
    if method == "laplacian":
        sets.append("levels=5")
    return tuple(sets)


def _run_current_baseline(case: Path, method: str, tile: int, work: Path, sets: tuple[str, ...], align: bool = False) -> dict[str, object]:
    """Run stack and export against the current append-only project format."""
    output = work / f"{method}-t{tile}"
    shutil.rmtree(output, ignore_errors=True)
    output.mkdir(parents=True)
    stack = [str(part) for part in ("/usr/bin/time", "-v", *gff.stack_cmd(str(case), method, tile, str(output), sets, align))]
    process = subprocess.run(stack, capture_output=True, text=True)
    if process.returncode:
        raise RuntimeError(f"stack exit {process.returncode}: {process.stderr[-1000:]}")
    report = json.loads(process.stdout)
    project = Path(report["outputs"][0])
    composite, zmap = output / "composite.tif", output / "zmap.tif"
    export = [str(ROOT / "target" / "debug" / "focusforge-cli"), "export", str(project), "-o", str(composite), "--zmap", str(zmap), "--json"]
    exported = subprocess.run(export, capture_output=True, text=True)
    if exported.returncode:
        raise RuntimeError(f"export exit {exported.returncode}: {exported.stderr[-1000:]}")
    rss = int(next(line for line in process.stderr.splitlines() if "Maximum resident set size" in line).split()[-1])
    return {"comp": str(composite), "zmap": str(zmap), "project": str(project), "rss_kb": rss,
            "wall_s": report["wall_seconds"], "tiles": report["tiles"],
            "levels": report["levels"], "margin": report["margin"],
            "reads_per_frame": report["frame_tiles_read"] / report["frame_count"],
            "accounted_bytes": report["accounted_bytes_in_flight"], "cmd": " ".join(stack[2:]),
            "export_cmd": " ".join(export), "stack_exit": process.returncode, "export_exit": exported.returncode,
            "stack_stdout": process.stdout, "stack_stderr": process.stderr, "export_stdout": exported.stdout, "export_stderr": exported.stderr,
            "equalization": report["equalization"], "alignment": report["align"]}


def _difference(left: Path, right: Path) -> dict[str, object]:
    a, b = gff.read_u16(left), gff.read_u16(right)
    delta = np.abs(a.astype(np.int64) - b.astype(np.int64))
    return {"sha256_equal": sha256_path(left) == sha256_path(right), "max_abs": int(delta.max()), "different_samples": int((delta != 0).sum())}


def _annotate_differences(rows: list[dict[str, object]]) -> None:
    repeats: dict[tuple[object, ...], list[dict[str, object]]] = defaultdict(list)
    tiles: dict[tuple[object, ...], list[dict[str, object]]] = defaultdict(list)
    for row in rows:
        repeats[(row["family"], row["member"], row["seed"], row["method"], row["tile"], row["equalized"])].append(row)
        tiles[(row["family"], row["member"], row["seed"], row["method"], row["repeat"], row["equalized"])].append(row)
    for group in repeats.values():
        if len(group) == 2:
            diff = _difference(Path(group[0]["composite"]), Path(group[1]["composite"]))
            for row in group:
                row["repeat_difference"] = diff
    for group in tiles.values():
        if len(group) == 2:
            group.sort(key=lambda row: int(row["tile"]))
            diff = _difference(Path(group[0]["composite"]), Path(group[1]["composite"]))
            for row in group:
                row["tile_difference"] = diff


def allowances(rows: list[dict[str, object]]) -> list[dict[str, object]]:
    """Derive per-metric seed and repeat spreads without mixing their sources."""
    grouped: dict[tuple[str, str, str, int, str], list[dict[str, object]]] = defaultdict(list)
    for row in rows:
        for metric, value in row["metrics"].items():
            if value is not None:
                grouped[(row["family"], row["member"], row["method"], row["tile"], metric)].append(row)
    output = []
    for key, records in sorted(grouped.items()):
        by_seed: dict[str, list[float]] = defaultdict(list)
        for row in records:
            by_seed[row["seed"]].append(float(row["metrics"][key[-1]]))
        seed_values = [values[0] for _, values in sorted(by_seed.items())]
        repeat_spread = max((max(values) - min(values) for values in by_seed.values()), default=0.0)
        seed_spread = max(seed_values) - min(seed_values) if seed_values else 0.0
        output.append({"family": key[0], "member": key[1], "method": key[2], "tile": key[3], "metric": key[4],
                       "seed_spread": seed_spread, "repeat_spread": repeat_spread, "allowance": max(seed_spread, repeat_spread)})
    return output


def baseline_call_count(seeds: Iterable[SeedTriple]) -> int:
    """Return the exact number of CPU baseline calls for a seed selection."""
    members = sum(len(family_members(family)) for family in FAMILIES if family.fusion_baseline)
    return len(tuple(seeds)) * members * len(METHODS) * len(TILES) * len(REPEATS)


def _record_metrics(data: SceneData, family: str, member: str, result: dict[str, object], composite: Path,
                    zmap: Path, source: object, boundaries: dict[str, object]) -> tuple[dict[str, float | None], dict[str, object]]:
    """Measure each record against the same truth, support, and family regions."""
    truth, depth, support = data.truth, data.depth, np.ones(data.depth.shape, dtype=bool)
    if family == "invalid-borders" and member == "aligned-coverage":
        truth, depth, support = _rebase_truth(data, int(result["alignment"]["reference"]))
    values = metrics.measure(gff.read_u16(composite), truth, data.metadata, source,
                             tuple(boundaries["vertical"]), tuple(boundaries["horizontal"]))
    unavailable: dict[str, str] = {}
    if family != "thin-hairs":
        values["hair_retention"] = None
        unavailable["hair_retention"] = "no generated hair region"
    if family != "flat-noise":
        values["flat_noise"] = None
        unavailable["flat_noise"] = "no generated near-flat region"
    labels = gff.read_zmap(zmap)
    values.update(metrics.label_metrics(labels, np.rint(depth).clip(0, 65535).astype(np.uint16), support & (labels != 65535)))
    if "tile_difference" in result:
        values["tile_difference"] = float(result["tile_difference"]["max_abs"])
    return values, {"metric_unavailable": unavailable, "label_truth_support_pixels": int(support.sum()),
                    "label_exact_half_ties": int((np.abs(depth - np.floor(depth)) == 0.5).sum()),
                    "label_boundary_ambiguity_pixels": int((~support).sum()) if family == "invalid-borders" and member == "aligned-coverage" else 0}


def _manifest(root: Path, rows: list[dict[str, object]], record_name: str) -> None:
    """Seal the runtime and every exported artifact beside the baseline records."""
    records = root / record_name
    payload = json.dumps(_json(rows), sort_keys=True, separators=(",", ":")).encode()
    runtime = provenance(())
    _write_json(root / f"{Path(record_name).stem}-manifest.json", {
        "record_count": len(rows), "record_sha256": sha256(payload).hexdigest(), "record_file": record_name,
        "record_file_sha256": sha256_path(records), "runtime": runtime,
        "captured_utc": datetime.now(timezone.utc).isoformat(),
        "artifacts": [{"case": row.get("case"), "method": row["method"], "tile": row["tile"], "repeat": row["repeat"],
                       "composite_sha256": row["composite_sha256"], "zmap_sha256": row["zmap_sha256"]} for row in rows],
    })


def run_baselines(root: Path, seeds: Iterable[SeedTriple], equalized: bool = False) -> list[dict[str, object]]:
    """Run every registered CPU baseline for the supplied non-sealed seeds."""
    cli = ROOT / "target" / "debug" / "focusforge-cli"
    if not cli.exists():
        raise FileNotFoundError(f"missing executable: {cli}")
    selected = tuple(seeds)
    expected = baseline_call_count(selected)
    rows: list[dict[str, object]] = []
    fusion_families = [family for family in FAMILIES if family.fusion_baseline]
    for family in fusion_families:
        for member in family_members(family):
            for seed in selected:
                if family.name == "invalid-borders" and member == "aligned-coverage":
                    rows.extend(_aligned_coverage_records(root, family, seed, equalized))
                    continue
                case, data, inputs = prepare_case(root, family.name, member, seed)
                source = _source(case)
                identity = provenance(inputs)
                for method in METHODS:
                    for tile in TILES:
                        for repeat in REPEATS:
                            work = root / "runs" / run_key(family.name + "-" + member, seed, method, tile, repeat)
                            result = _run_current_baseline(case, method, tile, work, _sets(method, equalized))
                            composite, zmap = Path(result["comp"]), Path(result["zmap"])
                            boundaries = tile_boundaries(data.truth.shape[1], data.truth.shape[0], tile, int(result["margin"]), int(result["levels"]))
                            row = {"case": str(case), "family": family.name, "member": member, "seed": seed.text(), "method": method, "tile": tile,
                                   "repeat": repeat, "equalized": equalized, "composite": str(composite), "zmap": str(zmap),
                                   "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap), "parameters": _sets(method, equalized),
                                   "tile_boundaries": boundaries, "attempt": {"repeat": repeat, "tile": tile, "method": method}, **identity, **result}
                            row["metrics"], record_metadata = _record_metrics(data, family.name, member, row, composite, zmap, source, boundaries)
                            row.update(record_metadata)
                            rows.append(row)
                            _write_json(root / "baseline.partial.json", rows)
    _annotate_differences(rows)
    for row in rows:
        row["metrics"]["tile_difference"] = float(row["tile_difference"]["max_abs"])
    if len(rows) != expected:
        raise AssertionError(f"expected {expected} baseline records, got {len(rows)}")
    record_name = "baseline-equalized.json" if equalized else "baseline.json"
    _write_json(root / record_name, rows)
    if not equalized:
        _write_json(root / "allowances.json", allowances(rows))
    _manifest(root, rows, record_name)
    return rows


def run_equalized_context(root: Path, seed: SeedTriple) -> list[dict[str, object]]:
    """Run one default-equalized context record per current method."""
    cli = ROOT / "target" / "debug" / "focusforge-cli"
    if not cli.exists():
        raise FileNotFoundError(f"missing executable: {cli}")
    family = next(family for family in FAMILIES if family.fusion_baseline)
    member = family_members(family)[0]
    case, data, inputs = prepare_case(root, family.name, member, seed)
    source, identity, rows = _source(case), provenance(inputs), []
    for method in METHODS:
        work = root / "context" / run_key(family.name + "-" + member, seed, method, 512, 0)
        result = _run_current_baseline(case, method, 512, work, _sets(method, True))
        composite, zmap = Path(result["comp"]), Path(result["zmap"])
        boundaries = tile_boundaries(data.truth.shape[1], data.truth.shape[0], 512, int(result["margin"]), int(result["levels"]))
        row = {"case": str(case), "family": family.name, "member": member, "seed": seed.text(), "method": method, "tile": 512, "repeat": 0,
                     "equalized": True, "composite": str(composite), "zmap": str(zmap), "composite_sha256": sha256_path(composite),
                     "zmap_sha256": sha256_path(zmap), "parameters": _sets(method, True), "tile_boundaries": boundaries,
                     "attempt": {"repeat": 0, "tile": 512, "method": method}, **identity, **result}
        row["metrics"], record_metadata = _record_metrics(data, family.name, member, row, composite, zmap, source, boundaries)
        row.update(record_metadata)
        rows.append(row)
    _write_json(root / "context.json", rows)
    _manifest(root, rows, "context.json")
    return rows


def normalize_context_records(root: Path) -> list[dict[str, object]]:
    """Apply registered family-region rules to context outputs."""
    path = root / "context.json"
    rows = json.loads(path.read_text())
    for row in rows:
        family = next(item for item in FAMILIES if item.name == row["family"])
        seed = SeedTriple.parse(row["seed"])
        case = _case_dir(root, family.name, row["member"], seed)
        data = render_family(family, seed, row["member"])
        source = _source(case)
        row["case"] = str(case)
        row["metrics"], record_metadata = _record_metrics(data, family.name, row["member"], row,
                                                             Path(row["composite"]), Path(row["zmap"]), source, row["tile_boundaries"])
        row.update(record_metadata)
    _write_json(path, rows)
    return rows


def _actual_aligned_source(result: dict[str, object], work: Path) -> tuple[object, list[Path], Path, list[str]]:
    """Export the CLI's aligned inputs and derive coverage from its report."""
    alignment = result.get("alignment")
    if not isinstance(alignment, dict) or not alignment.get("alignments"):
        raise AssertionError("aligned coverage run did not report transforms")
    frames_dir = work / "as-fused-frames"
    frames_dir.mkdir(parents=True, exist_ok=True)
    commands = []
    for index, _entry in enumerate(alignment["alignments"]):
        path = frames_dir / f"{index:02d}.tif"
        command = [str(ROOT / "target" / "debug" / "focusforge-cli"), "export", str(result["project"]),
                   "--frame", str(index), "--frame-output", str(path), "--json"]
        exported = subprocess.run(command, capture_output=True, text=True)
        if exported.returncode:
            raise RuntimeError(f"aligned frame export exit {exported.returncode}: {exported.stderr[-1000:]}")
        commands.append(" ".join(command))
    paths = gff.frame_paths(frames_dir)
    first = gff.read_u16(paths[0])
    masks = gff.product_masks(alignment["alignments"], first.shape[0], first.shape[1])
    mask_path = work / "actual-coverage.npy"
    np.save(mask_path, masks)
    source = gff.TiffSource(paths, mask_path)
    return source, paths, mask_path, commands


def _aligned_coverage_records(root: Path, family: object, seed: SeedTriple, equalized: bool) -> list[dict[str, object]]:
    """Run one registered coverage case with the source pixels the CLI actually fused."""
    case, data, inputs = prepare_case(root, family.name, "aligned-coverage", seed)
    identity, rows = provenance(inputs), []
    for method in METHODS:
        for tile in TILES:
            for repeat in REPEATS:
                work = root / "runs" / run_key(family.name + "-aligned-coverage", seed, method, tile, repeat)
                result = _run_current_baseline(case, method, tile, work, _sets(method, equalized), align=True)
                source, source_paths, mask_path, frame_commands = _actual_aligned_source(result, work)
                composite, zmap = Path(result["comp"]), Path(result["zmap"])
                boundaries = tile_boundaries(data.truth.shape[1], data.truth.shape[0], tile, int(result["margin"]), int(result["levels"]))
                row = {"case": str(case), "family": family.name, "member": "aligned-coverage", "seed": seed.text(),
                       "method": method, "tile": tile, "repeat": repeat, "equalized": equalized, "composite": str(composite), "zmap": str(zmap),
                       "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap), "parameters": _sets(method, equalized),
                       "tile_boundaries": boundaries, "attempt": {"repeat": repeat, "tile": tile, "method": method},
                       "actual_source_frames": [str(path) for path in source_paths], "actual_source_sha256": {Path(path).name: sha256_path(Path(path)) for path in source_paths},
                       "actual_coverage": str(mask_path), "actual_coverage_sha256": sha256_path(mask_path), "actual_frame_export_commands": frame_commands,
                       **identity, **result}
                row["metrics"], record_metadata = _record_metrics(data, family.name, "aligned-coverage", row, composite, zmap, source, boundaries)
                row.update(record_metadata)
                rows.append(row)
    return rows


def _rebase_truth(data: SceneData, reference: int) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Map known generated truth/depth into the CLI reference canvas."""
    from . import scenes
    transform = np.asarray(data.metadata["transforms"][reference], dtype=np.float64)
    truth = np.rint(scenes.ss.resample(data.truth, transform)).clip(0, 65535).astype(np.uint16)
    depth = scenes.ss.resample(data.depth[..., None], transform)[..., 0]
    support = scenes._coverage(transform)
    return truth, depth, support


def _twin_disagreements(actual: np.ndarray, twin: np.ndarray, scores: np.ndarray, eligible: np.ndarray,
                         tolerance: float) -> list[dict[str, object]]:
    """Reject invalid winners and score separations outside measured f32 order spread."""
    if actual.shape != twin.shape or scores.shape[1:] != actual.shape or eligible.shape != scores.shape:
        raise AssertionError("twin shapes disagree")
    frame_count = scores.shape[0]
    valid_actual = actual != fusion.NO_SOURCE
    if np.any(valid_actual & ((actual < 0) | (actual >= frame_count))):
        raise AssertionError("CLI Z-Map contains an invalid frame index")
    expected_empty = ~eligible.any(axis=0)
    if np.any((actual == fusion.NO_SOURCE) != expected_empty):
        raise AssertionError("CLI Z-Map eligibility disagrees with the twin")
    yy, xx = np.indices(actual.shape)
    if np.any(valid_actual & ~eligible[actual.clip(0, frame_count - 1), yy, xx]):
        raise AssertionError("CLI Z-Map selects an ineligible frame")
    records = []
    for y, x in zip(*np.nonzero(actual != twin)):
        actual_index, twin_index = int(actual[y, x]), int(twin[y, x])
        if actual_index == int(fusion.NO_SOURCE) or twin_index == int(fusion.NO_SOURCE):
            raise AssertionError("CLI and twin disagree on eligibility")
        margin = abs(float(scores[actual_index, y, x]) - float(scores[twin_index, y, x]))
        record = {"x": int(x), "y": int(y), "actual": actual_index, "twin": twin_index,
                  "actual_score_f32": float(scores[actual_index, y, x]), "twin_score_f32": float(scores[twin_index, y, x]),
                  "margin_f32": margin, "within_f32_order_tolerance": margin < tolerance}
        if margin >= tolerance:
            raise AssertionError(f"CLI/twin score margin {margin} exceeds f32 order tolerance {tolerance} at {x},{y}")
        records.append(record)
    return records


def run_focused_twins(root: Path) -> list[dict[str, object]]:
    """Six registered Details/Depth selection checks, including true coverage."""
    rows = json.loads((root / "baseline.json").read_text())
    wanted = {("thin-hairs", "default"), ("flat-noise", "noise-250"), ("invalid-borders", "aligned-coverage")}
    chosen = [r for r in rows if (r["family"], r["member"]) in wanted and r["seed"] == TRAINING_SEEDS[0].text()
              and r["method"] in ("laplacian", "depth-map") and r["tile"] == 1024 and r["repeat"] == 0]
    out = []
    for row in chosen:
        paths = row.get("actual_source_frames") or gff.frame_paths(Path(row["case"]))
        frames = np.stack([gff.read_u16(path) for path in paths])
        coverage = np.load(row["actual_coverage"]) if "actual_coverage" in row else None
        view = fusion.details_level0(frames, coverage) if row["method"] == "laplacian" else fusion.depth_map(frames, coverage)
        radius = 8 if row["method"] == "laplacian" else 4
        alternate = np.stack([fusion.score_plane_f32_alternate(detail, radius) for detail in view.details])
        if row["method"] == "depth-map":
            alternate = np.stack([fusion.score_plane_f32_alternate(score[..., None], 8) for score in alternate])
        alternate_winners = fusion.select(alternate, view.eligible)
        reference = np.stack([fusion.score_plane_f64_reference(detail, radius) for detail in view.details])
        if row["method"] == "depth-map":
            reference = np.stack([fusion.score_plane_f64_reference(score[..., None], 8) for score in reference])
        actual = gff.read_zmap(row["zmap"])
        tolerance = float(np.max(np.abs(alternate.astype(np.float64) - view.scores.astype(np.float64))))
        disagreements = _twin_disagreements(actual, view.winners, view.scores, view.eligible, tolerance)
        winner_copy: dict[str, object] | None = None
        if row["method"] == "depth-map":
            copied = fusion.gather(frames, actual)
            composite = gff.read_u16(row["composite"])
            delta = np.abs(copied.astype(np.int64) - composite.astype(np.int64))
            winner_copy = {"max_abs": int(delta.max()), "different_samples": int((delta != 0).sum()),
                           "exact_16bit_copy": bool(np.array_equal(copied, composite)), "no_source_is_zero": bool((copied[actual == fusion.NO_SOURCE] == 0).all())}
            if not winner_copy["exact_16bit_copy"] or not winner_copy["no_source_is_zero"]:
                raise AssertionError("Depth map winner copy is not exact")
        out.append({"family": row["family"], "member": row["member"], "method": row["method"], "seed": row["seed"],
                    "pixels": int(actual.size), "cli_twin_disagreements": len(disagreements), "disagreements": disagreements,
                    "alternate_f32_winner_changes": int((alternate_winners != view.winners).sum()),
                    "f32_summation_order_tolerance": tolerance,
                    "f64_roundoff_max_abs": float(np.max(np.abs(reference - view.scores.astype(np.float64)))),
                    "actual_coverage": row.get("actual_coverage"), "depth_winner_copy": winner_copy})
    if len(out) != 6: raise AssertionError(f"expected six focused twins, got {len(out)}")
    _write_json(root / "twins-focused.json", out)
    return out


def _focus_inputs(row: dict[str, object]) -> tuple[np.ndarray, np.ndarray | None]:
    paths = row.get("actual_source_frames") or gff.frame_paths(Path(str(row["case"])))
    frames = np.stack([gff.read_u16(path) for path in paths])
    coverage_path = row.get("actual_coverage")
    coverage = np.load(str(coverage_path)) if coverage_path else None
    return frames, coverage


def _focus_fuse(frames: np.ndarray, coverage: np.ndarray | None, method: str, score: object = None,
                eligibility_reach: int | None = None, score_radius: int = 4) -> fusion.FusionResult:
    if method == "laplacian":
        return fusion.details(frames, coverage, levels=5, radius=8, score=score)
    if method == "weighted-average":
        return fusion.weighted_average(frames, coverage, radius=16, score=score)
    if method == "depth-map":
        return fusion.depth_map_replay(frames, coverage, score_radius=score_radius, smooth_radius=8,
                                       eligibility_reach=eligibility_reach, score=score)
    raise ValueError(f"unknown fusion method: {method}")


def _tile_edges(length: int, boundaries: list[object]) -> list[int]:
    return [0, *(int(value) for value in boundaries), length]


def _tiled_focus(frames: np.ndarray, coverage: np.ndarray | None, method: str, vertical: list[object],
                 horizontal: list[object], margin: int, score: object = None,
                 eligibility_reach: int | None = None, score_radius: int = 4) -> fusion.FusionResult:
    """Fuse recorded interiors from halo-expanded tiles."""
    if not vertical and not horizontal:
        return _focus_fuse(frames, coverage, method, score, eligibility_reach, score_radius)
    height, width = frames.shape[1:3]
    composite = np.zeros_like(frames[0], dtype=np.uint16)
    winners = np.full((height, width), fusion.NO_SOURCE, dtype=np.uint16)
    eligible = np.zeros((frames.shape[0], height, width), dtype=bool)
    for y0, y1 in zip(_tile_edges(height, horizontal), _tile_edges(height, horizontal)[1:]):
        for x0, x1 in zip(_tile_edges(width, vertical), _tile_edges(width, vertical)[1:]):
            outer_y0, outer_y1 = max(0, y0 - margin), min(height, y1 + margin)
            outer_x0, outer_x1 = max(0, x0 - margin), min(width, x1 + margin)
            crop_coverage = None if coverage is None else coverage[:, outer_y0:outer_y1, outer_x0:outer_x1]
            crop = _focus_fuse(frames[:, outer_y0:outer_y1, outer_x0:outer_x1], crop_coverage, method, score, eligibility_reach, score_radius)
            local_y, local_x = slice(y0 - outer_y0, y1 - outer_y0), slice(x0 - outer_x0, x1 - outer_x0)
            composite[y0:y1, x0:x1] = crop.composite[local_y, local_x]
            winners[y0:y1, x0:x1] = crop.winners[local_y, local_x]
            eligible[:, y0:y1, x0:x1] = crop.eligible[:, local_y, local_x]
    return fusion.FusionResult(composite, winners, eligible)


def focus_tile_plan(row: dict[str, object], height: int, width: int, eligibility_reach: int | None = None,
                    score_radius: int = 4) -> dict[str, object]:
    """Expand a recorded plan only when a registered Depth Map arm needs more halo."""
    recorded = row["tile_boundaries"]
    if str(row["method"]) != "depth-map":
        return recorded
    required = max(int(recorded["margin"]), score_radius + 8, int(eligibility_reach or 0))
    if required == int(recorded["margin"]):
        return recorded
    planned = tile_boundaries(width, height, int(row["tile"]), required, int(recorded["levels"]))
    read = tile_read_bound(width, height, planned)
    if max(read.values()) > int(row["tile"]):
        raise AssertionError("candidate tile plan exceeds its registered tile size")
    return planned


def focus_replay(row: dict[str, object], score: object = None, eligibility_reach: int | None = None,
                 score_radius: int = 4, return_plan: bool = False) -> fusion.FusionResult | tuple[fusion.FusionResult, dict[str, object]]:
    """Run one registered method with a halo sufficient for the selected score and mask."""
    frames, coverage = _focus_inputs(row)
    plan = focus_tile_plan(row, frames.shape[1], frames.shape[2], eligibility_reach, score_radius)
    result = _tiled_focus(frames, coverage, str(row["method"]), list(plan["vertical"]), list(plan["horizontal"]),
                          int(plan["margin"]), score, eligibility_reach, score_radius)
    return (result, plan) if return_plan else result


def focus_tile_checks() -> list[dict[str, int | str]]:
    """Check candidate fusion against a forced 512-pixel interior split."""
    from . import focus_score

    random = np.random.default_rng(27)
    frames = random.integers(0, 65536, size=(2, 544, 544, 3), dtype=np.uint16)
    coverage = np.ones(frames.shape[:3], dtype=bool)
    coverage[:, 272, 272] = False
    checks: list[tuple[str, bool, dict[str, int]]] = []
    for method in METHODS:
        one = _focus_fuse(frames, coverage, method, focus_score.detrended)
        tiled = _tiled_focus(frames, coverage, method, [192, 384], [192, 384], 160, focus_score.detrended)
        delta = np.abs(one.composite.astype(np.int64) - tiled.composite.astype(np.int64))
        same_winners = np.array_equal(one.winners, tiled.winners)
        if method in ("weighted-average", "depth-map"):
            ok = bool(delta.max() == 0 and same_winners)
        else:
            ok = bool(delta.max() <= 1 and same_winners)
        checks.append((f"focus-tile512-{method}", ok, {"max_abs": int(delta.max()),
                                                          "different_samples": int((delta != 0).sum()),
                                                          "zmap_different_pixels": int((one.winners != tiled.winners).sum())}))
    radius10_frames = random.integers(0, 65536, size=(2, 1024, 1024, 3), dtype=np.uint16)
    radius10_coverage = np.ones(radius10_frames.shape[:3], dtype=bool)
    radius10_coverage[:, 512, 512] = False
    radius10_row = {"method": "depth-map", "tile": 512,
                    "tile_boundaries": tile_boundaries(1024, 1024, 512, 16, 1)}
    radius10_arms = (("r10-e16", focus_score.detrended, 10, 16, 18), ("r10-e18", focus_score.detrended, 10, 18, 18),
                     ("current-r4-e16", focus_score.current, 4, 16, 16), ("current-r4-e18", focus_score.current, 4, 18, 18))
    for name, score, radius, eligibility_reach, expected_margin in radius10_arms:
        radius10_plan = focus_tile_plan(radius10_row, 1024, 1024, eligibility_reach, radius)
        radius10_one = _focus_fuse(radius10_frames, radius10_coverage, "depth-map", score, eligibility_reach, radius)
        radius10_tiled = _tiled_focus(radius10_frames, radius10_coverage, "depth-map", list(radius10_plan["vertical"]),
                                      list(radius10_plan["horizontal"]), int(radius10_plan["margin"]), score, eligibility_reach, radius)
        radius10_delta = np.abs(radius10_one.composite.astype(np.int64) - radius10_tiled.composite.astype(np.int64))
        radius10_read = tile_read_bound(1024, 1024, radius10_plan)
        checks.append(("focus-tile512-r10-halo" if name == "r10-e18" else f"focus-tile512-{name}-halo",
                       bool(radius10_delta.max() == 0 and np.array_equal(radius10_one.winners, radius10_tiled.winners)
                            and np.array_equal(radius10_one.eligible, radius10_tiled.eligible)
                            and radius10_plan["margin"] == expected_margin
                            and (radius10_plan["interior"] == 476 if expected_margin == 18 else radius10_plan["interior"] == 480)
                            and max(radius10_read.values()) == 512),
                       {"max_abs": int(radius10_delta.max()), "different_samples": int((radius10_delta != 0).sum()),
                        "zmap_different_pixels": int((radius10_one.winners != radius10_tiled.winners).sum()),
                        "eligibility_different_pixels": int((radius10_one.eligible != radius10_tiled.eligible).sum()),
                        "max_read": max(radius10_read.values())}))
    failed = [name for name, ok, _values in checks if not ok]
    if failed:
        raise AssertionError(", ".join(failed))
    return [{"name": name, **values} for name, _ok, values in checks]


def replay_control_checks() -> list[str]:
    """Prove image, winner, and eligibility replay changes are rejected."""
    frames = np.arange(2 * 33 * 35 * 3, dtype=np.float32).reshape(2, 33, 35, 3)
    coverage = np.ones(frames.shape[:3], dtype=bool)
    replay = _focus_fuse(frames, coverage, "depth-map")
    checks = []
    for name, altered in (
        ("replay-image-rejected", fusion.FusionResult(replay.composite.copy(), replay.winners, replay.eligible)),
        ("replay-winner-rejected", fusion.FusionResult(replay.composite, replay.winners.copy(), replay.eligible)),
        ("replay-eligibility-rejected", fusion.FusionResult(replay.composite, replay.winners, replay.eligible.copy())),
    ):
        if name == "replay-image-rejected":
            altered.composite[0, 0, 0] += np.uint16(1)
        elif name == "replay-winner-rejected":
            altered.winners[0, 0] = fusion.NO_SOURCE
        else:
            altered.eligible[0, 0, 0] = False
        rejected = not (np.array_equal(replay.composite, altered.composite) and np.array_equal(replay.winners, altered.winners)
                        and np.array_equal(replay.eligible, altered.eligible))
        checks.append((name, rejected))
    failed = [name for name, ok in checks if not ok]
    if failed:
        raise AssertionError(", ".join(failed))
    return [name for name, _ok in checks]


def focus_configuration_checks() -> list[str]:
    """Exercise every registered configuration shape on a tiny known stack."""
    from . import focus_score

    frames = np.arange(2 * 35 * 37 * 3, dtype=np.float32).reshape(2, 35, 37, 3)
    coverage = np.ones(frames.shape[:3], dtype=bool)
    configurations = [("laplacian", "current", 8), ("laplacian", "detrended-std", 8), ("laplacian", "luma-laplacian", 8),
                      ("laplacian", "plain-variance", 8), ("weighted-average", "current", 16),
                      ("weighted-average", "detrended-std", 16), ("weighted-average", "luma-laplacian", 16),
                      ("weighted-average", "plain-variance", 16), ("depth-map", "current", 4),
                      ("depth-map", "detrended-std", 4), ("depth-map", "luma-laplacian", 4),
                      ("depth-map", "plain-variance", 4), ("depth-map", "detrended-std", 10),
                      ("depth-map", "detrended-std", 10), ("weighted-average", "adaptive-detrended-std", 16),
                      ("depth-map", "adaptive-detrended-std", 4), ("depth-map", "current", 4), ("depth-map", "current", 4)]
    for method, score_name, radius in configurations:
        score = focus_score.adaptive_detrended((4, 8, 16) if method == "weighted-average" else (1, 2, 4)) if score_name == "adaptive-detrended-std" else focus_score.score_by_name(score_name)
        result = _focus_fuse(frames, coverage, method, score, 18 if radius == 10 and method == "depth-map" else None, radius)
        if result.composite.shape != frames.shape[1:] or result.winners.shape != frames.shape[1:3]:
            raise AssertionError("registered focus configuration shape mismatch")
    return ["focus-configurations-18"]


def replay_focus_baselines(root: Path) -> list[dict[str, object]]:
    """Require exact current-score images and source maps before candidate rows."""
    rows = json.loads((root / "baseline.json").read_text())
    selected = [row for row in rows if row["repeat"] == 0]
    if len(selected) != 180:
        raise ValueError(f"expected 180 baseline replay rows, got {len(selected)}")
    records = []
    for row in selected:
        result = focus_replay(row)
        image = gff.read_u16(Path(str(row["composite"])))
        zmap = gff.read_zmap(Path(str(row["zmap"])))
        image_delta = np.abs(result.composite.astype(np.int64) - image.astype(np.int64))
        if image_delta.max() != 0 or not np.array_equal(result.winners, zmap):
            raise AssertionError(f"current replay mismatch: {row['family']}/{row['member']} {row['seed']} {row['method']} t{row['tile']}")
        details = None
        if row["method"] == "laplacian":
            frames, coverage = _focus_inputs(row)
            view = fusion.pyramid_views(frames, 5, coverage=coverage)
            details = [{"level": level, "score_radius": part.score_radius, "reach": part.reach,
                        "eligible_pixels": int(part.eligible.any(axis=0).sum()),
                        "winner_sha256": sha256(part.winners.tobytes()).hexdigest()}
                       for level, part in enumerate(view.levels)]
        records.append({"family": row["family"], "member": row["member"], "seed": row["seed"],
                        "method": row["method"], "tile": row["tile"], "composite_max_sample_difference": 0,
                        "zmap_equal": True, "eligible_pixels": int(result.eligible.any(axis=0).sum()),
                        "input_identity": row.get("input_sha256"), "command": row.get("cmd"),
                        "details_one_tile_intermediates": details})
    return records


def focus_configuration(row: dict[str, object], configuration: dict[str, object],
                        return_plan: bool = False) -> fusion.FusionResult | tuple[fusion.FusionResult, dict[str, object]]:
    """Run one registered candidate or control on one frozen baseline input."""
    from . import focus_score

    score_name = str(configuration["score"])
    score = focus_score.adaptive_detrended(tuple(configuration["radii"])) if score_name == "adaptive-detrended-std" else focus_score.score_by_name(score_name)
    method = str(configuration["method"])
    if method == "laplacian":
        return focus_replay(row, score=score, return_plan=return_plan)
    if method == "weighted-average":
        return focus_replay(row, score=score, return_plan=return_plan)
    if method == "depth-map":
        radius = int(configuration.get("score_radius", max(configuration.get("radii", (4,)))))
        prepared = dict(row)
        prepared["method"] = method
        return focus_replay(prepared, score, configuration.get("eligibility_reach"), radius, return_plan)
    raise ValueError(f"unknown fusion method: {method}")


def depth_full_score_reach(configuration: dict[str, object], radius: int) -> int:
    """Return the complete current or candidate Depth Map score footprint."""
    if "full_score_reach" in configuration:
        return int(configuration["full_score_reach"])
    return (2 * radius if str(configuration.get("score")) in ("current", "luma-laplacian") else radius) + 8


def run_focus_configurations(root: Path, output: Path, configurations: list[dict[str, object]], allowances_path: Path,
                             budget: dict[str, object]) -> list[dict[str, object]]:
    """Write per-input comparison rows after current replay has passed."""
    rows = json.loads((root / "baseline.json").read_text())
    selected = [row for row in rows if row["repeat"] == 0]
    allowance_rows = json.loads(allowances_path.read_text())
    runtime = provenance(())
    records = []
    for configuration in configurations:
        if time.monotonic() - float(budget["started_monotonic"]) >= 7200:
            budget["exhausted"] = True
            budget.setdefault("deferred_configurations", []).append(configuration["id"])
            return records
        for row in selected:
            if time.monotonic() - float(budget["started_monotonic"]) >= 7200:
                budget["exhausted"] = True
                budget.setdefault("deferred_configurations", []).append(configuration["id"])
                return records
            row_started = time.monotonic()
            if row["method"] != configuration["method"]:
                continue
            result, candidate_plan = focus_configuration(row, configuration, return_plan=True)
            if row.get("actual_coverage"):
                coverage = np.load(str(row["actual_coverage"]))
                if configuration["method"] == "weighted-average":
                    full_reach = 20
                elif configuration["method"] == "depth-map":
                    full_reach = depth_full_score_reach(configuration, int(configuration.get("score_radius", max(configuration.get("radii", (4,))))))
                else:
                    full_reach = None
                if full_reach is None:
                    details_support = [{"level": level, "reach": fusion.level_reach(level, 8),
                                        "eligible_pixels": int(fusion.per_level_eligible(coverage, level, 8).any(axis=0).sum())}
                                       for level in range(5)]
                    required = fusion.per_level_eligible(coverage, 0, 8)
                else:
                    details_support = None
                    required = np.stack([fusion.eligible(mask, full_reach) for mask in coverage])
            else:
                required = np.ones_like(result.eligible, dtype=bool)
                details_support = None
            unsupported_by_frame = (result.eligible & ~required).sum(axis=(1, 2)).astype(int).tolist()
            unsupported_support_count = int(sum(unsupported_by_frame))
            if configuration.get("can_advance") and unsupported_support_count:
                raise AssertionError(f"unsafe candidate support: {configuration['id']}")
            directory = output / str(configuration["id"]) / run_key(f"{row['family']}-{row['member']}", SeedTriple.parse(str(row["seed"])), str(row["method"]), int(row["tile"]), 0)
            directory.mkdir(parents=True, exist_ok=True)
            composite, zmap = directory / "composite.tif", directory / "zmap.tif"
            gff.write_u16(composite, result.composite)
            gff.write_u16(zmap, result.winners[..., None])
            family = next(item for item in FAMILIES if item.name == row["family"])
            data = render_family(family, SeedTriple.parse(str(row["seed"])), str(row["member"]))
            metric_row = {**row, "comp": str(composite), "zmap": str(zmap), "tile_boundaries": candidate_plan}
            metric_row.pop("tile_difference", None)
            source = gff.TiffSource(row["actual_source_frames"], row["actual_coverage"]) if "actual_source_frames" in row else _source(Path(str(row["case"])))
            image_metrics, record_metadata = _record_metrics(data, str(row["family"]), str(row["member"]), metric_row,
                                                               composite, zmap, source, candidate_plan)
            label = {name: image_metrics.pop(name) for name in ("label_exact", "label_within_one", "label_mad")}
            records.append({"configuration_id": configuration["id"], "matched_control": configuration.get("matched_control"),
                            "classification": configuration.get("classification", "control"), "can_advance": configuration.get("can_advance", False),
                            "family": row["family"], "member": row["member"], "seed": row["seed"], "method": row["method"], "tile": row["tile"],
                            "eligibility_pixels": int(result.eligible.any(axis=0).sum()), "excluded_pixels": int((~result.eligible.any(axis=0)).sum()),
                            "composite": str(composite), "zmap": str(zmap), "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap),
                            "image_metrics": image_metrics, "label_metrics": label, **record_metadata,
                            "unavailable_metrics": {name: "unavailable for this input" for name, value in image_metrics.items() if value is None},
                            "unsupported_support_count": unsupported_support_count, "input_identity": row.get("input_sha256"),
                            "baseline_composite_sha256": row.get("composite_sha256"), "runtime": runtime,
                            "unsupported_support_by_frame": unsupported_by_frame, "wall_seconds": time.monotonic() - row_started,
                            "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                            "configuration": configuration, "details_support_guard": details_support,
                            "tile_boundaries": candidate_plan,
                            "tile_read_bound": tile_read_bound(result.composite.shape[1], result.composite.shape[0], candidate_plan),
                            "allowance_keys": [item for item in allowance_rows if item["family"] == row["family"] and item["member"] == row["member"]
                                               and item["method"] == row["method"] and item["tile"] == row["tile"]]})
    groups: dict[tuple[object, ...], list[dict[str, object]]] = defaultdict(list)
    for record in records:
        groups[(record["configuration_id"], record["family"], record["member"], record["seed"], record["method"])].append(record)
    for group in groups.values():
        by_tile = {int(record["tile"]): record for record in group}
        if set(by_tile) != {512, 1024}:
            continue
        first, second = by_tile[512], by_tile[1024]
        image_delta = np.abs(gff.read_u16(Path(str(first["composite"]))).astype(np.int64) - gff.read_u16(Path(str(second["composite"]))).astype(np.int64))
        zmap_delta = gff.read_zmap(Path(str(first["zmap"]))) != gff.read_zmap(Path(str(second["zmap"])))
        tile_difference = {"max_abs": int(image_delta.max()), "different_samples": int((image_delta != 0).sum()),
                           "zmap_different_pixels": int(zmap_delta.sum())}
        for record in group:
            record["tile_difference"] = tile_difference
            record["image_metrics"]["tile_difference"] = float(tile_difference["max_abs"])
    return records


def focus_support_records(records: list[dict[str, object]]) -> list[dict[str, object]]:
    """Compare boundary arms and controls only where both selected a source."""
    output = []
    for candidate_id in ("depth-map-detrended-std-r10-e16", "depth-map-detrended-std-r10-e18"):
        candidate_rows = [record for record in records if record["configuration_id"] == candidate_id]
        for candidate in candidate_rows:
            control_id = str(candidate["matched_control"])
            control = next((record for record in records if record["configuration_id"] == control_id
                            and all(record.get(key) == candidate.get(key) for key in ("family", "member", "seed", "tile", "method"))), None)
            if control is None:
                continue
            candidate_map = gff.read_zmap(Path(str(candidate["zmap"])))
            control_map = gff.read_zmap(Path(str(control["zmap"])))
            common = (candidate_map != fusion.NO_SOURCE) & (control_map != fusion.NO_SOURCE)
            image_delta = np.abs(gff.read_u16(Path(str(candidate["composite"]))).astype(np.int64)
                                 - gff.read_u16(Path(str(control["composite"]))).astype(np.int64))
            candidate_metrics = candidate.get("image_metrics", {})
            control_metrics = control.get("image_metrics", {})
            output.append({"candidate": candidate_id, "matched_control": control_id, "family": candidate.get("family"),
                           "member": candidate.get("member"), "seed": candidate.get("seed"), "tile": candidate.get("tile"),
                           "common_eligible_pixels": int(common.sum()), "candidate_eligible_pixels": candidate["eligibility_pixels"],
                           "control_eligible_pixels": control["eligibility_pixels"], "excluded_pixels": candidate["excluded_pixels"],
                           "winner_changes": int((candidate_map[common] != control_map[common]).sum()),
                           "unsupported_support_count": candidate["unsupported_support_count"],
                           "common_image_max_abs": int(image_delta[common].max()) if common.any() else 0,
                           "common_image_different_samples": int((image_delta[common] != 0).sum()),
                           "border_contamination_delta": None if candidate_metrics.get("border_contamination") is None or control_metrics.get("border_contamination") is None
                           else float(candidate_metrics["border_contamination"] - control_metrics["border_contamination"]),
                           "border_contamination_scope": "full eligible image" if candidate_metrics else "unavailable without truth"})
    e16 = [record for record in records if record["configuration_id"] == "depth-map-detrended-std-r10-e16"]
    for diagnostic in e16:
        guarded = next((record for record in records if record["configuration_id"] == "depth-map-detrended-std-r10-e18"
                        and all(record.get(key) == diagnostic.get(key) for key in ("family", "member", "seed", "tile", "method"))), None)
        if guarded is None:
            continue
        diagnostic_map = gff.read_zmap(Path(str(diagnostic["zmap"])))
        guarded_map = gff.read_zmap(Path(str(guarded["zmap"])))
        common = (diagnostic_map != fusion.NO_SOURCE) & (guarded_map != fusion.NO_SOURCE)
        image_delta = np.abs(gff.read_u16(Path(str(diagnostic["composite"]))).astype(np.int64)
                             - gff.read_u16(Path(str(guarded["composite"]))).astype(np.int64))
        output.append({"comparison": "r10-e16-vs-e18", "family": diagnostic.get("family"), "member": diagnostic.get("member"),
                       "seed": diagnostic.get("seed"), "tile": diagnostic.get("tile"), "common_eligible_pixels": int(common.sum()),
                       "e16_excluded_pixels": diagnostic["excluded_pixels"], "e18_excluded_pixels": guarded["excluded_pixels"],
                       "winner_changes": int((diagnostic_map[common] != guarded_map[common]).sum()),
                       "common_image_max_abs": int(image_delta[common].max()) if common.any() else 0,
                       "common_image_different_samples": int((image_delta[common] != 0).sum()),
                       "e16_unsupported_support_count": diagnostic["unsupported_support_count"],
                       "e18_unsupported_support_count": guarded["unsupported_support_count"]})
    return output


def run_focus_stack(root: Path, source: str, prepared: Path, rect_text: str | None,
                    configurations: list[dict[str, object]], budget: dict[str, object]) -> dict[str, object]:
    """Replay current and registered scores from one validated aligned real crop."""
    from . import focus_score

    rect = _registered_rect(source, rect_text)
    directory, inputs, coverage_proof = _crop_prepared_inputs(root, source, prepared, rect)
    manifest = _validate_preparation(prepared, source)
    frames = np.stack([gff.read_u16(path) for path in inputs])
    coverage = np.ones(frames.shape[:3], dtype=bool)
    retained_path = prepared.parents[3] / "stacking-crops" / source / "crops.json"
    retained = json.loads(retained_path.read_text()) if retained_path.is_file() else []
    retained_by_method = {str(row.get("method")): row for row in retained
                          if row.get("prepared_manifest_sha256") == sha256_path(prepared)
                          and row.get("alignment_report_sha256") == manifest["alignment_report_sha256"]
                          and row.get("rect_xywh") == list(rect)}
    if set(retained_by_method) != set(METHODS):
        raise ValueError("retained current real replays do not match the prepared crop")
    started = time.monotonic()
    runtime = provenance(inputs)
    rows = []
    for configuration in configurations:
        if time.monotonic() - float(budget["started_monotonic"]) >= 7200:
            budget["exhausted"] = True
            budget.setdefault("deferred_configurations", []).append(configuration["id"])
            continue
        method = str(configuration["method"])
        score_name = str(configuration["score"])
        score = focus_score.adaptive_detrended(tuple(configuration["radii"])) if score_name == "adaptive-detrended-std" else focus_score.score_by_name(score_name)
        reach = configuration.get("eligibility_reach")
        radius = int(configuration.get("score_radius", max(configuration.get("radii", (4,))))) if method == "depth-map" else 4
        row_started = time.monotonic()
        result = _focus_fuse(frames, coverage, method, score, reach, radius)
        if method == "weighted-average":
            full_reach, details_support = 20, None
        elif method == "depth-map":
            full_reach, details_support = depth_full_score_reach(configuration, radius), None
        else:
            full_reach = None
            details_support = [{"level": level, "reach": fusion.level_reach(level, 8),
                                "eligible_pixels": int(fusion.per_level_eligible(coverage, level, 8).any(axis=0).sum())}
                               for level in range(5)]
        required = (np.stack([fusion.eligible(mask, full_reach) for mask in coverage]) if full_reach is not None
                    else fusion.per_level_eligible(coverage, 0, 8))
        unsupported_by_frame = (result.eligible & ~required).sum(axis=(1, 2)).astype(int).tolist()
        unsupported_support_count = int(sum(unsupported_by_frame))
        if configuration.get("can_advance") and unsupported_support_count:
            raise AssertionError(f"unsafe real candidate support: {configuration['id']}")
        work = root / "stack" / str(configuration["id"])
        work.mkdir(parents=True, exist_ok=True)
        composite, zmap = work / "composite.tif", work / "zmap.tif"
        gff.write_u16(composite, result.composite)
        gff.write_u16(zmap, result.winners[..., None])
        replay = None
        if score_name == "current" and "eligibility_reach" not in configuration:
            baseline = retained_by_method[method]
            image_delta = np.abs(result.composite.astype(np.int64) - gff.read_u16(Path(str(baseline["composite"]))).astype(np.int64))
            zmap_equal = np.array_equal(result.winners, gff.read_zmap(Path(str(baseline["zmap"]))))
            if image_delta.max() != 0 or not zmap_equal:
                raise AssertionError(f"current real replay mismatch: {method}")
            replay = {"composite_max_sample_difference": 0, "zmap_equal": True,
                      "retained_composite_sha256": baseline["composite_sha256"], "retained_zmap_sha256": baseline["zmap_sha256"]}
        rows.append({"configuration_id": configuration["id"], "matched_control": configuration.get("matched_control"),
                     "classification": configuration.get("classification", "control"), "can_advance": configuration.get("can_advance", False),
                     "method": method, "score": score_name, "rect_xywh": list(rect), "coverage": coverage_proof,
                     "eligibility_pixels": int(result.eligible.any(axis=0).sum()), "excluded_pixels": int((~result.eligible.any(axis=0)).sum()),
                     "composite": str(composite), "zmap": str(zmap), "composite_sha256": sha256_path(composite),
                     "zmap_sha256": sha256_path(zmap), "truth_metrics": "not computed", "replay": replay,
                     "wall_seconds": time.monotonic() - row_started,
                     "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss, "runtime": runtime,
                     "configuration": configuration, "score_radius": radius, "eligibility_reach": reach,
                     "full_score_reach": full_reach, "details_support_guard": details_support,
                     "unsupported_support_by_frame": unsupported_by_frame, "unsupported_support_count": unsupported_support_count,
                     "image_metrics": {}})
    montages = []
    montage_pairs = [(method, f"{method}-current", f"{method}-detrended-std") for method in METHODS]
    montage_pairs.extend((("depth-map-r10-e16", "depth-map-current-r4-e16", "depth-map-detrended-std-r10-e16"),
                          ("depth-map-r10-e18", "depth-map-current-r4-e18", "depth-map-detrended-std-r10-e18")))
    by_id = {str(row["configuration_id"]): row for row in rows}
    for name, current_id, candidate_id in montage_pairs:
        current, candidate = by_id.get(current_id), by_id.get(candidate_id)
        if current is not None and candidate is not None:
            montage = root / "stack" / f"{name}-current-vs-candidate.png"
            gff.montage(str(montage), (0, rect[3], 0, rect[2]), [Path(str(current["composite"])), Path(str(candidate["composite"]))])
            montages.append({"comparison": name, "path": str(montage), "sha256": sha256_path(montage)})
    return {"source": source, "rect_xywh": list(rect), "prepared_manifest": str(prepared.resolve()),
            "prepared_manifest_sha256": sha256_path(prepared), "alignment_report_sha256": manifest["alignment_report_sha256"],
            "raw_inputs": manifest["raw_inputs"], "aligned_inputs": manifest["aligned_inputs"], "coverage": coverage_proof,
            "input_sha256": {path.name: sha256_path(path) for path in inputs}, "rows": rows, "montages": montages,
            "wall_seconds": time.monotonic() - started, "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss}


def index_calibration(root: Path) -> dict[str, object]:
    """Measure the registered flat-noise p99 once before field candidates."""
    rows = json.loads((root / "baseline.json").read_text())
    row = next(item for item in rows if item["repeat"] == 0 and item["method"] == "depth-map"
               and item["family"] == "flat-noise" and item["member"] == "noise-250"
               and item["seed"] == TRAINING_SEEDS[0].text() and item["tile"] == 512)
    frames, coverage = _focus_inputs(row)
    inputs = index_field.field_inputs(frames, coverage)
    family = next(item for item in FAMILIES if item.name == "flat-noise")
    data = render_family(family, TRAINING_SEEDS[0], "noise-250")
    y0, y1, x0, x1 = data.metadata["flat"]
    region = np.where(inputs.eligibility[:, y0 + 8:y1 - 8, x0 + 8:x1 - 8], inputs.raw_scores[:, y0 + 8:y1 - 8, x0 + 8:x1 - 8], -np.inf)
    values = np.max(region, axis=0)
    finite = values[np.isfinite(values)]
    if not finite.size:
        raise ValueError("calibration noise interior has no eligible raw-score samples")
    p99 = float(np.percentile(finite, 99, method="linear"))
    return {"family": "flat-noise", "member": "noise-250", "seed": TRAINING_SEEDS[0].text(),
            "region_yx": [y0 + 8, y1 - 8, x0 + 8, x1 - 8], "eligible_count": int(finite.size),
            "raw_score_digest": sha256(finite.astype(np.float32).tobytes()).hexdigest(), "p99": p99,
            "method": "linear", "thresholds": {str(multiplier): p99 * multiplier for multiplier in (1, 2, 4)}}


def _index_tiled(frames: np.ndarray, coverage: np.ndarray | None, threshold: float, rung: str,
                 plan: dict[str, object]) -> index_field.FieldResult:
    """Build a field per recorded halo tile without changing the fixed margin."""
    vertical, horizontal, margin = list(plan["vertical"]), list(plan["horizontal"]), int(plan["margin"])
    if not vertical and not horizontal:
        return index_field.build_field(frames, coverage, threshold, rung)
    height, width = frames.shape[1:3]
    composite = np.zeros_like(frames[0], dtype=np.uint16)
    field = np.zeros((height, width), dtype=np.float32)
    trusted = np.zeros((height, width), dtype=bool)
    filled = np.zeros((height, width), dtype=bool)
    unresolved = np.zeros((height, width), dtype=bool)
    fallback = np.zeros((height, width), dtype=bool)
    no_source = np.zeros((height, width), dtype=bool)
    for y0, y1 in zip(_tile_edges(height, horizontal), _tile_edges(height, horizontal)[1:]):
        for x0, x1 in zip(_tile_edges(width, vertical), _tile_edges(width, vertical)[1:]):
            oy0, oy1, ox0, ox1 = max(0, y0 - margin), min(height, y1 + margin), max(0, x0 - margin), min(width, x1 + margin)
            crop_coverage = None if coverage is None else coverage[:, oy0:oy1, ox0:ox1]
            result = index_field.build_field(frames[:, oy0:oy1, ox0:ox1], crop_coverage, threshold, rung)
            local_y, local_x = slice(y0 - oy0, y1 - oy0), slice(x0 - ox0, x1 - ox0)
            target = np.s_[y0:y1, x0:x1]
            composite[target], field[target] = result.composite[local_y, local_x], result.field[local_y, local_x]
            trusted[target], filled[target], unresolved[target] = result.trusted[local_y, local_x], result.filled[local_y, local_x], result.unresolved[local_y, local_x]
            fallback[target], no_source[target] = result.adjacent_fallback[local_y, local_x], result.no_source[local_y, local_x]
    return index_field.FieldResult(composite, field, trusted, filled, unresolved, fallback, no_source)


def index_replay_controls(root: Path) -> list[dict[str, object]]:
    """Require exact C0 replay on the 60 retained Depth Map rows."""
    rows = json.loads((root / "baseline.json").read_text())
    records = []
    for row in (item for item in rows if item["repeat"] == 0 and item["method"] == "depth-map"):
        frames, coverage = _focus_inputs(row)
        result = index_field.build_field(frames, coverage, 0.0, "C0")
        image, zmap = gff.read_u16(Path(str(row["composite"]))), gff.read_zmap(Path(str(row["zmap"])))
        delta = np.abs(result.composite.astype(np.int64) - image.astype(np.int64))
        if delta.max() != 0 or not np.array_equal(np.rint(result.field).astype(np.uint16), zmap):
            raise AssertionError(f"index C0 replay mismatch: {row['family']}/{row['member']} {row['seed']} t{row['tile']}")
        records.append({"family": row["family"], "member": row["member"], "seed": row["seed"], "tile": row["tile"],
                        "image_max_abs": 0, "zmap_equal": True})
    if len(records) != 60:
        raise AssertionError(f"expected 60 depth-map replay rows, got {len(records)}")
    return records


def index_field_tile_checks() -> list[dict[str, int | str]]:
    """Exercise forced tiled field construction without asserting false parity."""
    random = np.random.default_rng(2703)
    frames = random.integers(0, 65536, size=(2, 96, 96, 3), dtype=np.uint16)
    coverage = np.ones(frames.shape[:3], dtype=bool)
    coverage[:, 40:56, 40:56] = False
    plan = tile_boundaries(96, 96, 64, 16, 1)
    whole = index_field.build_field(frames, coverage, 0.0, "A3")
    tiled = _index_tiled(frames, coverage, 0.0, "A3", plan)
    delta = np.abs(whole.composite.astype(np.int64) - tiled.composite.astype(np.int64))
    untrusted = _index_tiled(frames, np.ones_like(coverage), float("inf"), "A3", plan)
    c0 = _index_tiled(frames, np.ones_like(coverage), 0.0, "C0", plan)
    if not np.array_equal(untrusted.composite, c0.composite) or not untrusted.unresolved.all():
        raise AssertionError("forced tiled unresolved fallback control failed")
    all_unknown = _index_tiled(frames, np.zeros_like(coverage), 0.0, "A3", plan)
    if not all_unknown.no_source.all() or all_unknown.composite.any():
        raise AssertionError("forced tiled all-unknown control failed")
    ramp = np.add.outer(np.arange(96, dtype=np.float32), np.arange(96, dtype=np.float32))
    trusted = np.ones((96, 96), dtype=bool)
    trusted[40:56, 40:56] = False
    ramp_whole = index_field.fill_indices(ramp, trusted)
    ramp_tiled = np.zeros_like(ramp)
    for y0, y1 in zip(_tile_edges(96, list(plan["horizontal"])), _tile_edges(96, list(plan["horizontal"]))[1:]):
        for x0, x1 in zip(_tile_edges(96, list(plan["vertical"])), _tile_edges(96, list(plan["vertical"]))[1:]):
            oy0, oy1, ox0, ox1 = max(0, y0 - 16), min(96, y1 + 16), max(0, x0 - 16), min(96, x1 + 16)
            filled = index_field.fill_indices(ramp[oy0:oy1, ox0:ox1], trusted[oy0:oy1, ox0:ox1])
            ramp_tiled[y0:y1, x0:x1] = filled.field[y0 - oy0:y1 - oy0, x0 - ox0:x1 - ox0]
    ramp_delta = np.abs(ramp_whole.field - ramp_tiled)
    return [{"name": "index-field-forced-tile", "max_abs": int(delta.max()),
             "different_samples": int((delta != 0).sum()), "field_different_pixels": int((whole.field != tiled.field).sum()),
             "ramp_hole_max_abs": float(ramp_delta[40:56, 40:56].max()),
             "vertical_origins": ",".join(str(value) for value in plan["vertical"]),
             "horizontal_origins": ",".join(str(value) for value in plan["horizontal"]), "margin": int(plan["margin"])}]


def _field_partitions(result: index_field.FieldResult, depth: np.ndarray, truth_support: np.ndarray) -> dict[str, object]:
    output: dict[str, object] = {}
    for name, mask in (("trusted", result.trusted), ("filled", result.filled), ("unresolved", result.unresolved)):
        mask = mask & ~result.no_source & truth_support
        values = np.abs(result.field[mask] - depth[mask])
        output[name] = ({"count": int(values.size), "mean_abs": float(values.mean()),
                         "p90_abs": float(np.percentile(values, 90, method="linear"))}
                        if values.size else {"count": 0, "mean_abs": None, "p90_abs": None, "reason": "empty partition"})
    return output


def _index_tile_phase(row: dict[str, object], plan: dict[str, object], height: int, width: int) -> dict[str, object]:
    origin = row.get("global_origin_xy", [0, 0])
    if not isinstance(origin, list) or len(origin) != 2:
        origin = [0, 0]
    margin = int(plan["margin"])
    reads = []
    for y0, y1 in zip(_tile_edges(height, list(plan["horizontal"])), _tile_edges(height, list(plan["horizontal"]))[1:]):
        for x0, x1 in zip(_tile_edges(width, list(plan["vertical"])), _tile_edges(width, list(plan["vertical"]))[1:]):
            ox0, oy0 = max(0, x0 - margin), max(0, y0 - margin)
            ox1, oy1 = min(width, x1 + margin), min(height, y1 + margin)
            global_x, global_y = int(origin[0]) + ox0, int(origin[1]) + oy0
            reads.append({"global_xywh": [global_x, global_y, ox1 - ox0, oy1 - oy0],
                          "phase_xy": [global_x % 8, global_y % 8]})
    return {"global_origin_xy": origin, "period": 8, "read_windows": reads,
            "vertical_cut_x": [int(value) for value in plan["vertical"]],
            "horizontal_cut_y": [int(value) for value in plan["horizontal"]], "margin": margin}


def _index_dependency_bounds(rung: str, margin: int) -> dict[str, int | bool]:
    if rung in {"C0", "C0r", "N1"}:
        sample, coverage = 16, 16
    elif rung == "A1":
        sample, coverage = 16, 22
    elif rung in {"A2", "A3"}:
        sample, coverage = 44, 50
    elif rung == "A4":
        sample, coverage = 44, 44
    else:
        raise ValueError(f"unknown index-field rung: {rung}")
    return {"sample_read_reach": sample, "coverage_mask_reach": coverage, "fixed_tile_margin": margin,
            "sample_within_margin": sample <= margin, "coverage_within_margin": coverage <= margin}


def run_index_configurations(root: Path, output: Path, configurations: list[dict[str, object]], allowances_path: Path,
                             calibration: dict[str, object], budget: dict[str, object], family_filter: str | None = None) -> list[dict[str, object]]:
    """Run field controls and registered candidates over retained Depth Map inputs."""
    rows = [item for item in json.loads((root / "baseline.json").read_text()) if item["repeat"] == 0 and item["method"] == "depth-map"
            and (family_filter is None or item["family"] == family_filter)]
    allowances = json.loads(allowances_path.read_text())
    records: list[dict[str, object]] = []
    for configuration in configurations:
        multiplier = int(configuration.get("threshold_multiplier", 1))
        threshold = float(calibration["thresholds"].get(str(multiplier), 0.0))
        for row in rows:
            if time.monotonic() - float(budget["started_monotonic"]) >= 7200:
                budget["exhausted"] = True
                budget.setdefault("deferred_configurations", []).append(configuration["id"])
                return records
            started = time.monotonic()
            frames, coverage = _focus_inputs(row)
            plan = row["tile_boundaries"]
            rung = str(configuration["rung"])
            result = _index_tiled(frames, coverage, threshold, rung, plan)
            whole = index_field.build_field(frames, coverage, threshold, rung)
            directory = output / str(configuration["id"]) / run_key(f"{row['family']}-{row['member']}", SeedTriple.parse(str(row["seed"])), "depth-map", int(row["tile"]), 0)
            directory.mkdir(parents=True, exist_ok=True)
            composite, zmap, field_path = directory / "composite.tif", directory / "zmap.tif", directory / "field.npy"
            gff.write_u16(composite, result.composite)
            labels = np.where(result.no_source, fusion.NO_SOURCE, np.rint(result.field).clip(0, 65535)).astype(np.uint16)
            gff.write_u16(zmap, labels[..., None])
            np.save(field_path, result.field)
            family = next(item for item in FAMILIES if item.name == row["family"])
            data = render_family(family, SeedTriple.parse(str(row["seed"])), str(row["member"]))
            image_delta = np.abs(result.composite.astype(np.int64) - whole.composite.astype(np.int64))
            field_delta = np.abs(result.field - whole.field)
            rounded_delta = np.where(result.no_source, fusion.NO_SOURCE, np.rint(result.field).clip(0, 65535)).astype(np.uint16) != np.where(
                whole.no_source, fusion.NO_SOURCE, np.rint(whole.field).clip(0, 65535)
            ).astype(np.uint16)
            tile_difference = {"max_abs": int(image_delta.max()), "different_samples": int((image_delta != 0).sum()),
                               "field_max_abs": float(field_delta.max()), "field_different_pixels": int((field_delta != 0).sum()),
                               "rounded_label_different_pixels": int(rounded_delta.sum()),
                               **_index_tile_phase(row, plan, result.composite.shape[0], result.composite.shape[1])}
            metric_row = {**row, "comp": str(composite), "zmap": str(zmap), "tile_boundaries": plan,
                          "tile_difference": tile_difference}
            source = gff.TiffSource(row["actual_source_frames"], row["actual_coverage"]) if "actual_source_frames" in row else _source(Path(str(row["case"])))
            image_metrics, metadata = _record_metrics(data, str(row["family"]), str(row["member"]), metric_row, composite, zmap, source, plan)
            label = {name: image_metrics.pop(name) for name in ("label_exact", "label_within_one", "label_mad")}
            partition_depth, partition_support = data.depth, np.ones(data.depth.shape, dtype=bool)
            if row["family"] == "invalid-borders" and row["member"] == "aligned-coverage":
                _, partition_depth, partition_support = _rebase_truth(data, int(row["alignment"]["reference"]))
            records.append({"configuration_id": configuration["id"], "rung": rung, "matched_control": configuration.get("matched_control"),
                            "classification": configuration.get("classification", "control"), "can_advance": configuration.get("can_advance", False),
                            "family": row["family"], "member": row["member"], "seed": row["seed"], "method": "depth-map", "tile": row["tile"],
                            "threshold": threshold, "eligibility_pixels": int((~result.no_source).sum()), "excluded_pixels": int(result.no_source.sum()),
                            "trusted_pixels": int(result.trusted.sum()), "filled_pixels": int(result.filled.sum()), "unresolved_pixels": int(result.unresolved.sum()),
                            "adjacent_fallback_pixels": int(result.adjacent_fallback.sum()), "composite": str(composite), "zmap": str(zmap), "field": str(field_path),
                            "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap), "field_sha256": sha256_path(field_path),
                            "image_metrics": image_metrics, "label_metrics": label,
                            "continuous_partitions": _field_partitions(result, partition_depth, partition_support),
                            "tile_difference": tile_difference,
                            "tile_boundaries": plan, "tile_read_bound": tile_read_bound(result.composite.shape[1], result.composite.shape[0], plan),
                            "dependency_bounds": _index_dependency_bounds(rung, int(plan["margin"])),
                            "wall_seconds": time.monotonic() - started, "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                            "input_identity": row.get("input_sha256"), "baseline_composite_sha256": row.get("composite_sha256"),
                            "allowance_keys": [item for item in allowances if item["family"] == row["family"] and item["member"] == row["member"] and item["method"] == "depth-map" and item["tile"] == row["tile"]], **metadata})
    return records


def _selection_budget_remaining(budget: dict[str, object], deferred: str) -> bool:
    """Stop at the registered session boundary and retain the completed rows."""
    if time.monotonic() - float(budget["started_monotonic"]) < float(budget["session_cap_seconds"]):
        return True
    budget["exhausted"] = True
    budget.setdefault("deferred", []).append(deferred)
    return False


def run_selection_replay(root: Path, budget: dict[str, object] | None = None) -> list[dict[str, object]]:
    """Replay retained Details rows through the bounded streaming identity path."""
    rows = [row for row in json.loads((root / "baseline.json").read_text())
            if row["repeat"] == 0 and row["method"] == "laplacian"]
    records = []
    for row in rows:
        if budget is not None and not _selection_budget_remaining(budget, "retained Details replay"):
            return records
        source = _selection_source(row)
        result, counters = consistency.fuse_streaming(source, "identity")
        composite = gff.read_u16(Path(str(row["composite"])))
        zmap = gff.read_zmap(Path(str(row["zmap"])))
        image_delta = np.abs(result.composite.astype(np.int64) - composite.astype(np.int64))
        if image_delta.max() != 0 or not np.array_equal(result.zmap, zmap):
            raise AssertionError("streaming identity does not match retained Details output")
        records.append({"family": row["family"], "member": row["member"], "seed": row["seed"], "tile": row["tile"],
                        "composite_sha256": row["composite_sha256"], "zmap_sha256": row["zmap_sha256"],
                        "raw_winner_sha256": [sha256(plane.tobytes()).hexdigest() for plane in result.raw_winners],
                        "final_winner_sha256": [sha256(plane.tobytes()).hexdigest() for plane in result.final_winners],
                        "stream_reads": counters})
    if len(records) != 60:
        raise AssertionError("registered Details replay must contain 60 rows")
    return records


def selection_streaming_resource_controls(root: Path) -> list[dict[str, object]]:
    """Run separate TIFF-backed child controls without constructing an all-frame array."""
    rows = json.loads((root / "baseline.json").read_text())
    row = next((item for item in rows if item["family"] == "thin-hairs" and item["method"] == "laplacian"
                and item["repeat"] == 0 and item["tile"] == 512), None)
    if row is None:
        raise ValueError("retained Details source is required for resource controls")
    case = (ROOT / str(row["case"])).resolve()
    script = """
import json
import resource
import sys
sys.path.insert(0, "spikes/guided-filter-fusion")
from spikes.stacking_experiments import consistency
import gff_fusion as gff
paths = gff.frame_paths(sys.argv[1])
count = int(sys.argv[2])
source = gff.TiffSource((paths * ((count + len(paths) - 1) // len(paths)))[:count])
_result, counters = consistency.fuse_streaming(source, "identity")
print(json.dumps({"frame_count": count, "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                  "sample_reads": counters["sample_reads"], "extra_mask_pass_reads": counters["extra_mask_pass_reads"],
                  "total_mask_reads": counters["total_mask_reads"]}, sort_keys=True))
"""
    records = []
    for count in (2, 8, 16):
        process = subprocess.run([sys.executable, "-c", script, str(case), str(count)], cwd=ROOT, capture_output=True, text=True)
        if process.returncode:
            raise RuntimeError(f"streaming resource control exit {process.returncode}: {process.stderr[-1000:]}")
        try:
            record = json.loads(process.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeError("streaming resource control did not return JSON") from error
        records.append(record)
    if [item.get("frame_count") for item in records if isinstance(item, dict)] != [2, 8, 16]:
        raise RuntimeError("streaming resource controls are incomplete")
    for record in records:
        if record["sample_reads"] != record["frame_count"] * 2 or record["extra_mask_pass_reads"] != 0:
            raise AssertionError("streaming resource controls do not report two sample passes")
    return records


def selection_tile_self_checks() -> list[dict[str, object]]:
    """Force odd partial reads beyond the current 192-pixel tile interior."""
    height, width = 545, 547
    ramp = np.arange(height * width, dtype=np.uint32).reshape(height, width)
    frames = np.stack([np.stack([(ramp + 911 * frame + 37 * channel) % 65536
                                 for channel in range(3)], axis=-1).astype(np.uint16)
                       for frame in range(3)])
    masks = np.ones((3, height, width), dtype=bool)
    masks[1, 273, 271] = False
    source = gff.ArraySource(frames, masks)
    records = []
    for rule, expanded_margin in (("identity", 160), ("majority-all", 192), ("isolated-all", 192),
                                  ("cross-scale", 192), ("destructive", 224)):
        whole, _whole_counts = consistency.fuse_streaming(source, rule)
        current, current_counts, current_plan = consistency.fuse_tiled(source, rule, 512, 160)
        expanded, expanded_counts, expanded_plan = consistency.fuse_tiled(source, rule, 512, expanded_margin)
        current_delta = _selection_tile_difference(whole, current)
        expanded_delta = _selection_tile_difference(whole, expanded)
        current_exact = rule != "destructive"
        if current_exact and any(value != 0 for value in (current_delta["max_abs"], current_delta["zmap_different_pixels"],
                                                            *current_delta["per_level_winner_different_pixels"], *current_delta["per_level_raw_different_pixels"])):
            raise AssertionError(f"current tile parity failed for {rule}")
        if any(value != 0 for value in (expanded_delta["max_abs"], expanded_delta["zmap_different_pixels"],
                                        *expanded_delta["per_level_winner_different_pixels"], *expanded_delta["per_level_raw_different_pixels"])):
            raise AssertionError(f"expanded tile parity failed for {rule}")
        windows = current_plan["windows"]
        if len(windows) < 2 or not any(window["read_xywh"][2] < width or window["read_xywh"][3] < height for window in windows):
            raise AssertionError("forced tile control did not contain a partial read")
        records.append({"name": f"selection-tile-{rule}", "shape": [height, width],
                        "mask_pixels": int((~masks).sum()), "current_160": {"plan": current_plan, "difference": current_delta, "stream_reads": current_counts},
                        "expanded": {"plan": expanded_plan, "difference": expanded_delta, "stream_reads": expanded_counts}})
    return records


def selection_width_allowances(rows: list[dict[str, object]]) -> dict[str, object]:
    """Derive the fixed width guards from retained thin-hair baseline measurements."""
    if len(rows) != 20:
        raise ValueError("width calibration requires exactly 20 retained baseline rows")
    seeds = {seed.text() for seed in TRAINING_SEEDS}
    grouped: dict[tuple[int, int, str], list[tuple[str, int, float]]] = defaultdict(list)
    bindings = []
    for row in rows:
        if row.get("family") != "thin-hairs" or row.get("method") != "laplacian" or int(row.get("tile", 0)) not in TILES:
            raise ValueError("width calibration row is not a retained thin-hair Details row")
        seed, repeat = str(row.get("seed")), int(row.get("repeat", -1))
        if seed not in seeds or repeat not in REPEATS or not isinstance(row.get("input_sha256"), dict) or not isinstance(row.get("composite_sha256"), str) or not all(
                isinstance(row.get(key), str) and row[key] for key in ("zmap_sha256", "segment_mask_sha256", "support_mask_sha256", "normalization")):
            raise ValueError("width calibration input binding is incomplete")
        values = row.get("width_metrics")
        if not isinstance(values, dict):
            raise ValueError("width calibration row lacks width metrics")
        for width in (1, 2):
            metrics_for_width = values.get(str(width))
            if not isinstance(metrics_for_width, dict):
                raise ValueError("width calibration is missing one width")
            for metric in ("hair_retention", "hair_label_exact"):
                value = metrics_for_width.get(metric)
                if not isinstance(value, (int, float)) or not math.isfinite(float(value)):
                    raise ValueError("width calibration metric is invalid")
                grouped[(width, int(row["tile"]), metric)].append((seed, repeat, float(value)))
        bindings.append({"seed": seed, "tile": int(row["tile"]), "repeat": repeat, "input_sha256": row["input_sha256"],
                         "composite_sha256": row["composite_sha256"], "zmap_sha256": row.get("zmap_sha256"),
                         "segment_mask_sha256": row.get("segment_mask_sha256"), "support_mask_sha256": row.get("support_mask_sha256"),
                         "normalization": row.get("normalization")})
    allowances = []
    for key in sorted(grouped):
        samples = grouped[key]
        if len(samples) != 10 or {seed for seed, _repeat, _value in samples} != seeds:
            raise ValueError("width calibration samples are incomplete")
        by_seed: dict[str, dict[int, float]] = defaultdict(dict)
        for seed, repeat, value in samples:
            if repeat in by_seed[seed]:
                raise ValueError("width calibration has duplicate seed/repeat values")
            by_seed[seed][repeat] = value
        if any(set(values) != set(REPEATS) for values in by_seed.values()):
            raise ValueError("width calibration repeat pairing is incomplete")
        seed_values = [values[0] for values in by_seed.values()]
        seed_spread = max(seed_values) - min(seed_values)
        repeat_spread = max(abs(values[0] - values[1]) for values in by_seed.values())
        allowances.append({"width": key[0], "tile": key[1], "metric": key[2], "direction": "higher",
                           "seed_spread": seed_spread, "repeat_spread": repeat_spread,
                           "allowance": max(seed_spread, repeat_spread),
                           "per_seed_repeat_values": {seed: values for seed, values in sorted(by_seed.items())}})
    if len(allowances) != 8:
        raise ValueError("width calibration must produce eight guard rows")
    return {"state": "measured", "allowances": allowances, "inputs": bindings}


def selection_width_measurements(root: Path) -> list[dict[str, object]]:
    """Measure fixed one- and two-pixel hair guards from retained Details outputs."""
    rows = [row for row in json.loads((root / "baseline.json").read_text())
            if row["family"] == "thin-hairs" and row["method"] == "laplacian"]
    measured = []
    family = next(item for item in FAMILIES if item.name == "thin-hairs")
    for row in rows:
        data = render_family(family, SeedTriple.parse(str(row["seed"])), str(row["member"]))
        image, labels = gff.read_u16(Path(str(row["composite"]))), gff.read_zmap(Path(str(row["zmap"])))
        values: dict[str, dict[str, float]] = {}
        masks: dict[str, str] = {}
        support = (labels != fusion.NO_SOURCE) & np.isfinite(data.depth)
        for width in (1, 2):
            segments = [hair for hair in data.metadata["hairs"] if abs(int(hair[1][1]) - int(hair[1][0])) == width]
            mask = np.zeros(labels.shape, dtype=bool)
            for kind, line, _before, _after, extent in segments:
                if kind == "h":
                    mask[line[0]:line[1], extent[0]:extent[1]] = True
                else:
                    mask[extent[0]:extent[1], line[0]:line[1]] = True
            reference = gff.hair_contrast(data.truth.mean(2), segments)
            observed = gff.hair_contrast(image.mean(2), segments)
            values[str(width)] = {"hair_retention": float((observed / reference).mean()),
                                  "hair_label_exact": float((labels[mask & support] == np.rint(data.depth[mask & support]).astype(np.uint16)).mean())}
            masks[str(width)] = sha256(mask.tobytes()).hexdigest()
        measured.append({**row, "width_metrics": values, "segment_mask_sha256": sha256(json.dumps(masks, sort_keys=True).encode()).hexdigest(), "segment_masks_sha256": masks,
                         "support_mask_sha256": sha256(support.tobytes()).hexdigest(),
                         "normalization": "gff.hair_contrast observed/reference by exact line width"})
    return measured


def _selection_source(row: dict[str, object]) -> object:
    """Open one retained Details input without loading its frame set at once."""
    if "actual_source_frames" in row:
        return gff.TiffSource(row["actual_source_frames"], row.get("actual_coverage"))
    return gff.TiffSource(gff.frame_paths(Path(str(row["case"]))))


def _selection_rule(configuration: dict[str, object]) -> str:
    """Translate registered names to the bounded selection implementation."""
    rules = {"majority-3x3-all": "majority-all", "isolated-all": "isolated-all",
             "cross-scale-raw-parent": "cross-scale", "isolated-level0": "isolated-level0",
             "identity": "identity", "mode-9x9-all": "destructive"}
    try:
        return rules[str(configuration["rule"])]
    except KeyError as error:
        raise ValueError("configuration has no tiled consistency rule") from error


def _selection_width_metrics(data: SceneData, result: consistency.SelectionResult,
                             baseline_labels: np.ndarray) -> tuple[dict[str, dict[str, float]], dict[str, str]]:
    """Measure each fixed hair width using the same contrast normalization as calibration."""
    values: dict[str, dict[str, float]] = {}
    hashes: dict[str, str] = {}
    support = (baseline_labels != fusion.NO_SOURCE) & np.isfinite(data.depth)
    labels = np.rint(data.depth).clip(0, 65535).astype(np.uint16)
    for width in (1, 2):
        hairs = [hair for hair in data.metadata["hairs"] if abs(int(hair[1][1]) - int(hair[1][0])) == width]
        mask = np.zeros(result.zmap.shape, dtype=bool)
        for kind, line, _before, _after, extent in hairs:
            if kind == "h":
                mask[line[0]:line[1], extent[0]:extent[1]] = True
            else:
                mask[extent[0]:extent[1], line[0]:line[1]] = True
        active = mask & support
        reference = gff.hair_contrast(data.truth.mean(2), hairs)
        observed = gff.hair_contrast(result.composite.mean(2), hairs)
        values[str(width)] = {"hair_retention": float((observed / reference).mean()),
                              "hair_label_exact": float((result.zmap[active] == labels[active]).mean())}
        hashes[str(width)] = sha256(mask.tobytes()).hexdigest()
    return values, hashes


def _selection_tile_difference(one: consistency.SelectionResult, tiled: consistency.SelectionResult) -> dict[str, object]:
    """Keep image, level map and raw-map equality separate in the audit record."""
    image = np.abs(one.composite.astype(np.int64) - tiled.composite.astype(np.int64))
    levels = [int(np.count_nonzero(left != right)) for left, right in zip(one.final_winners, tiled.final_winners)]
    raw = [int(np.count_nonzero(left != right)) for left, right in zip(one.raw_winners, tiled.raw_winners)]
    return {"max_abs": int(image.max()), "different_samples": int((image != 0).sum()),
            "zmap_different_pixels": int(np.count_nonzero(one.zmap != tiled.zmap)),
            "per_level_winner_different_pixels": levels, "per_level_raw_different_pixels": raw}


def _selection_map_audit(result: consistency.SelectionResult) -> dict[str, object]:
    """Record immutable and final maps without treating either as an image metric."""
    return {"raw_winner_sha256": [sha256(plane.tobytes()).hexdigest() for plane in result.raw_winners],
            "final_winner_sha256": [sha256(plane.tobytes()).hexdigest() for plane in result.final_winners],
            "per_level_changed_coefficients": consistency.changed_counts(list(result.raw_winners), list(result.final_winners))}


def _selection_result_record(output: Path, row: dict[str, object], configuration: dict[str, object],
                             result: consistency.SelectionResult, counters: dict[str, int],
                             plan: dict[str, object] | None, tile_measurements: dict[str, object] | None,
                             allowances: list[dict[str, object]]) -> dict[str, object]:
    """Persist one configuration row and its truth metrics when the input is synthetic."""
    directory = output / str(configuration["id"]) / run_key(f"{row['family']}-{row['member']}",
                                                               SeedTriple.parse(str(row["seed"])), "laplacian",
                                                               int(row["tile"]), int(row["repeat"]))
    directory.mkdir(parents=True, exist_ok=True)
    composite, zmap = directory / "composite.tif", directory / "zmap.tif"
    gff.write_u16(composite, result.composite)
    gff.write_u16(zmap, result.zmap[..., None])
    raw_hashes = [sha256(plane.tobytes()).hexdigest() for plane in result.raw_winners]
    final_hashes = [sha256(plane.tobytes()).hexdigest() for plane in result.final_winners]
    record: dict[str, object] = {"configuration_id": configuration["id"], "rule": configuration["rule"],
        "matched_candidate": configuration.get("matched_candidate"), "classification": configuration["classification"],
        "can_advance": configuration["can_advance"], "family": row["family"], "member": row["member"],
        "seed": row["seed"], "tile": row["tile"], "repeat": row["repeat"], "method": "laplacian",
        "composite": str(composite), "zmap": str(zmap), "composite_sha256": sha256_path(composite),
        "zmap_sha256": sha256_path(zmap), "raw_winner_sha256": raw_hashes, "final_winner_sha256": final_hashes,
        "per_level_changed_coefficients": consistency.changed_counts(list(result.raw_winners), list(result.final_winners)),
        "stream_reads": counters, "input_identity": row.get("input_sha256"),
        "baseline_composite_sha256": row.get("composite_sha256"), "baseline_zmap_sha256": row.get("zmap_sha256"),
        "tile_boundaries": plan, "tile_measurements": tile_measurements,
        "allowance_keys": [item for item in allowances if item["family"] == row["family"] and item["member"] == row["member"]
                           and item["method"] == "laplacian" and item["tile"] == row["tile"]],
        "wall_seconds": 0.0, "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss}
    family = next(item for item in FAMILIES if item.name == row["family"])
    data = render_family(family, SeedTriple.parse(str(row["seed"])), str(row["member"]))
    source = _selection_source(row)
    metric_row = {**row, "comp": str(composite), "zmap": str(zmap), "tile_boundaries": plan or row["tile_boundaries"]}
    metric_row.pop("tile_difference", None)
    if tile_measurements is not None:
        metric_row["tile_difference"] = tile_measurements["current_160"]["difference"]
    image_metrics, metadata = _record_metrics(data, str(row["family"]), str(row["member"]), metric_row,
                                               composite, zmap, source, metric_row["tile_boundaries"])
    record["label_metrics"] = {key: image_metrics.pop(key) for key in ("label_exact", "label_within_one", "label_mad")}
    record["image_metrics"] = image_metrics
    record["unavailable_metrics"] = metadata["metric_unavailable"]
    if tile_measurements is None:
        record["tile_difference"] = None
        record["tile_difference_unavailable"] = "global random control has no finite tile experiment"
    else:
        record["tile_difference"] = tile_measurements["current_160"]["difference"]
    if row["family"] == "thin-hairs":
        baseline_labels = gff.read_zmap(Path(str(row["zmap"])))
        widths, masks = _selection_width_metrics(data, result, baseline_labels)
        record["width_metrics"] = widths
        record["width_segment_masks_sha256"] = masks
        record["width_support_mask_sha256"] = sha256(((baseline_labels != fusion.NO_SOURCE) & np.isfinite(data.depth)).tobytes()).hexdigest()
    return {**record, **metadata}


def run_selection_configurations(root: Path, output: Path, configurations: list[dict[str, object]],
                                 controls: list[dict[str, object]], allowances_path: Path,
                                 family_filter: str | None = None, budget: dict[str, object] | None = None) -> list[dict[str, object]]:
    """Evaluate all registered consistency outputs from one immutable Details source per key."""
    all_rows = json.loads((root / "baseline.json").read_text())
    rows = [row for row in all_rows if row["method"] == "laplacian" and row["repeat"] == 0
            and (family_filter is None or row["family"] == family_filter)]
    expected = 60 if family_filter is None else 20
    if len(rows) != expected:
        raise AssertionError(f"selection input count is {len(rows)}, expected {expected}")
    allowances = json.loads(allowances_path.read_text())
    registered = [*controls[:2], *configurations, *controls[2:]]
    records: list[dict[str, object]] = []
    for row in rows:
        if budget is not None and not _selection_budget_remaining(budget, f"{row['family']}/{row['member']}/{row['seed']}/t{row['tile']}"):
            return records
        case_started = time.monotonic()
        source = _selection_source(row)
        results = consistency.fuse_configurations(source, configurations, controls, str(row["family"]), str(row["member"]), str(row["seed"]))
        shared_selection_seconds = time.monotonic() - case_started
        if set(results) != {str(item["id"]) for item in registered}:
            raise AssertionError("selection result configuration set does not match registration")
        persisted_counts: dict[str, list[int]] = {}
        for configuration in registered:
            if budget is not None and not _selection_budget_remaining(budget, f"{configuration['id']} after {row['family']}/{row['seed']}"):
                return records
            measurement_started = time.monotonic()
            result, counters = results[str(configuration["id"])]
            plan = None
            tile_measurements = None
            if configuration["rule"] != "matched-random":
                current, current_counts, current_plan = consistency.fuse_tiled(source, _selection_rule(configuration), int(row["tile"]), 160)
                expanded_margin = int(configuration["expanded_margin"])
                expanded, expanded_counts, expanded_plan = consistency.fuse_tiled(source, _selection_rule(configuration), int(row["tile"]), expanded_margin)
                plan = current_plan
                tile_measurements = {"whole": _selection_map_audit(result),
                                     "current_160": {"difference": _selection_tile_difference(result, current), "maps": _selection_map_audit(current), "stream_reads": current_counts, "plan": current_plan},
                                     "expanded": {"difference": _selection_tile_difference(result, expanded), "maps": _selection_map_audit(expanded), "stream_reads": expanded_counts, "plan": expanded_plan}}
                current_difference = tile_measurements["current_160"]["difference"]
                expanded_difference = tile_measurements["expanded"]["difference"]
                if configuration["rule"] != "mode-9x9-all" and any(value != 0 for value in (
                        current_difference["max_abs"], current_difference["zmap_different_pixels"],
                        *current_difference["per_level_winner_different_pixels"], *current_difference["per_level_raw_different_pixels"])):
                    raise AssertionError(f"current tile parity failed for {configuration['id']}")
                if any(value != 0 for value in (expanded_difference["max_abs"], expanded_difference["zmap_different_pixels"],
                                                 *expanded_difference["per_level_winner_different_pixels"], *expanded_difference["per_level_raw_different_pixels"])):
                    raise AssertionError(f"expanded tile parity failed for {configuration['id']}")
                result, counters = current, current_counts
                persisted_counts[str(configuration["id"])] = consistency.changed_counts(list(result.raw_winners), list(result.final_winners))
            else:
                matched = str(configuration["matched_candidate"])
                matched_counts = persisted_counts.get(matched)
                actual = consistency.changed_counts(list(result.raw_winners), list(result.final_winners))
                if matched_counts is None or actual != matched_counts:
                    raise AssertionError(f"paired random counts do not match {matched}")
            record = _selection_result_record(output, row, configuration, result, counters, plan, tile_measurements, allowances)
            if configuration["rule"] == "matched-random":
                record["matched_per_level_changed_coefficients"] = persisted_counts[str(configuration["matched_candidate"])]
            record["shared_selection_seconds"] = shared_selection_seconds
            record["measurement_seconds"] = time.monotonic() - measurement_started
            record["case_elapsed_seconds"] = time.monotonic() - case_started
            records.append(record)
    if budget is None or not budget["exhausted"]:
        if len(records) != expected * 10:
            raise AssertionError("selection output count does not match registered matrix")
    return records


def run_selection_stack(output: Path, source_name: str, prepared: Path, rect_text: str | None,
                        configurations: list[dict[str, object]], controls: list[dict[str, object]]) -> dict[str, object]:
    """Evaluate ten registered outputs on one strict aligned crop without synthetic truth claims."""
    rect = _registered_rect(source_name, rect_text)
    manifest = _validate_preparation(prepared, source_name)
    directory, paths, coverage = _crop_prepared_inputs(output, source_name, prepared, rect)
    source = gff.TiffSource(paths)
    results = consistency.fuse_configurations(source, configurations, controls, source_name, "aligned-roi", "registered")
    registered = [*controls[:2], *configurations, *controls[2:]]
    rows = []
    for configuration in registered:
        result, counters = results[str(configuration["id"])]
        work = directory / str(configuration["id"])
        work.mkdir(parents=True, exist_ok=True)
        composite, zmap = work / "composite.tif", work / "zmap.tif"
        gff.write_u16(composite, result.composite)
        gff.write_u16(zmap, result.zmap[..., None])
        rows.append({"configuration_id": configuration["id"], "rule": configuration["rule"],
                     "classification": configuration["classification"], "can_advance": configuration["can_advance"],
                     "rect_xywh": list(rect), "global_origin_xy": coverage["global_origin_xy"], "coverage": coverage,
                     "prepared_manifest_sha256": sha256_path(prepared), "alignment_report_sha256": manifest["alignment_report_sha256"],
                     "raw_inputs": manifest["raw_inputs"], "aligned_inputs": manifest["aligned_inputs"],
                     "composite": str(composite), "zmap": str(zmap), "composite_sha256": sha256_path(composite),
                     "zmap_sha256": sha256_path(zmap), "raw_winner_sha256": [sha256(part.tobytes()).hexdigest() for part in result.raw_winners],
                     "final_winner_sha256": [sha256(part.tobytes()).hexdigest() for part in result.final_winners],
                     "per_level_changed_coefficients": consistency.changed_counts(list(result.raw_winners), list(result.final_winners)),
                     "stream_reads": counters, "truth_metrics": "not computed"})
    if len(rows) != 10:
        raise AssertionError("real selection output count does not match registration")
    by_id = {str(row["configuration_id"]): row for row in rows}
    for row in rows:
        if row["rule"] != "matched-random":
            continue
        matched = next(item for item in controls if item["id"] == row["configuration_id"])["matched_candidate"]
        expected = by_id[str(matched)]["per_level_changed_coefficients"]
        if row["per_level_changed_coefficients"] != expected:
            raise AssertionError(f"real paired random counts do not match {matched}")
        row["matched_per_level_changed_coefficients"] = expected
    identity = next(row for row in rows if row["configuration_id"] == "consistency-identity")
    retained_path = prepared.parents[3] / "stacking-crops" / source_name / "crops.json"
    retained = json.loads(retained_path.read_text()) if retained_path.is_file() else []
    current = next((row for row in retained if row.get("method") == "laplacian"
                    and row.get("prepared_manifest_sha256") == sha256_path(prepared)
                    and row.get("rect_xywh") == list(rect)), None)
    if current is None:
        raise ValueError("retained current Details crop does not match the preparation")
    image_delta = np.abs(gff.read_u16(Path(identity["composite"])).astype(np.int64)
                         - gff.read_u16(Path(str(current["composite"]))).astype(np.int64))
    labels_equal = np.array_equal(gff.read_zmap(Path(identity["zmap"])), gff.read_zmap(Path(str(current["zmap"]))))
    if image_delta.max() != 0 or not labels_equal:
        raise AssertionError("real streaming identity does not match retained current Details crop")
    identity["retained_current_replay"] = {"composite_max_sample_difference": 0, "zmap_equal": True,
                                           "retained_composite_sha256": current["composite_sha256"],
                                           "retained_zmap_sha256": current["zmap_sha256"]}
    montages = []
    for row in rows:
        if row["classification"] == "candidate":
            image = output / "stack" / f"{row['configuration_id']}-current-vs-candidate.png"
            image.parent.mkdir(parents=True, exist_ok=True)
            gff.montage(str(image), (0, rect[3], 0, rect[2]), [Path(identity["composite"]), Path(row["composite"])])
            montages.append({"configuration_id": row["configuration_id"], "path": str(image), "sha256": sha256_path(image)})
    return {"source": source_name, "rect_xywh": list(rect), "prepared_manifest": str(prepared.resolve()),
            "prepared_manifest_sha256": sha256_path(prepared), "alignment_report_sha256": manifest["alignment_report_sha256"],
            "coverage": coverage, "rows": rows, "montages": montages}


def run_index_full_references(root: Path, output: Path, configurations: list[dict[str, object]],
                              calibration: dict[str, object], budget: dict[str, object]) -> list[dict[str, object]]:
    """Write the fixed whole-image three-coarse-visit diagnostic subset."""
    rows = [item for item in json.loads((root / "baseline.json").read_text()) if item["repeat"] == 0
            and item["method"] == "depth-map" and item["family"] == "thin-hairs" and item["member"] == "default"
            and item["seed"] == TRAINING_SEEDS[0].text() and item["tile"] == 512]
    if len(rows) != 1:
        raise AssertionError("full-depth reference input is not uniquely registered")
    row = rows[0]
    frames, coverage = _focus_inputs(row)
    records = []
    for configuration in (item for item in configurations if item["rung"] in {"A1", "A2", "A3"}):
        if time.monotonic() - float(budget["started_monotonic"]) >= 7200:
            budget["exhausted"] = True
            budget.setdefault("deferred_full_references", []).append(configuration["id"])
            return records
        threshold = float(calibration["thresholds"][str(configuration["threshold_multiplier"])])
        depth = index_field.full_fill_depth(frames.shape[1], frames.shape[2])
        result = index_field.build_field(frames, coverage, threshold, str(configuration["rung"]), fill_depth=depth)
        work = output / "full-reference" / str(configuration["id"])
        work.mkdir(parents=True, exist_ok=True)
        composite, zmap, field_path = work / "composite.tif", work / "zmap.tif", work / "field.npy"
        gff.write_u16(composite, result.composite)
        labels = np.where(result.no_source, fusion.NO_SOURCE, np.rint(result.field).clip(0, 65535)).astype(np.uint16)
        gff.write_u16(zmap, labels[..., None])
        np.save(field_path, result.field)
        records.append({"configuration_id": configuration["id"], "rung": configuration["rung"], "threshold": threshold,
                        "reference_only": True, "can_advance": False, "family": row["family"], "member": row["member"],
                        "seed": row["seed"], "tile": row["tile"], "fill_depth": depth, "coarse_visits": index_field.FILL_COARSE_VISITS,
                        "sweeps_per_visit": index_field.FILL_SWEEPS, "composite": str(composite), "zmap": str(zmap), "field": str(field_path),
                        "dependency_bounds": {"sample_read_reach": "whole-image", "coverage_mask_reach": "whole-image",
                                              "fixed_tile_margin": None, "reference_only": True},
                        "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap), "field_sha256": sha256_path(field_path),
                        "trusted_pixels": int(result.trusted.sum()), "filled_pixels": int(result.filled.sum()),
                        "unresolved_pixels": int(result.unresolved.sum())})
    return records


def run_index_stack(root: Path, source: str, prepared: Path, rect_text: str | None,
                    configurations: list[dict[str, object]], calibration: dict[str, object]) -> dict[str, object]:
    """Evaluate C0 and A3 field variants on one validated aligned real crop."""
    rect = _registered_rect(source, rect_text)
    manifest = _validate_preparation(prepared, source)
    retained_path = prepared.parents[3] / "stacking-crops" / source / "crops.json"
    retained = json.loads(retained_path.read_text()) if retained_path.is_file() else []
    retained_c0 = next((row for row in retained if row.get("method") == "depth-map"
                        and row.get("prepared_manifest_sha256") == sha256_path(prepared)
                        and row.get("alignment_report_sha256") == manifest["alignment_report_sha256"]
                        and row.get("rect_xywh") == list(rect)), None)
    if retained_c0 is None:
        raise ValueError("retained current Depth Map crop does not match the preparation")
    _directory, inputs, coverage_proof = _crop_prepared_inputs(root, source, prepared, rect)
    frames = np.stack([gff.read_u16(path) for path in inputs])
    coverage = np.ones(frames.shape[:3], dtype=bool)
    selected = [item for item in configurations if item["rung"] == "C0" or item["rung"] == "A3"]
    rows = []
    for configuration in selected:
        multiplier = int(configuration.get("threshold_multiplier", 1))
        threshold = float(calibration["thresholds"].get(str(multiplier), 0.0))
        result = index_field.build_field(frames, coverage, threshold, str(configuration["rung"]))
        work = root / "stack" / str(configuration["id"])
        work.mkdir(parents=True, exist_ok=True)
        composite, zmap, field_path = work / "composite.tif", work / "zmap.tif", work / "field.npy"
        gff.write_u16(composite, result.composite)
        labels = np.where(result.no_source, fusion.NO_SOURCE, np.rint(result.field).clip(0, 65535)).astype(np.uint16)
        gff.write_u16(zmap, labels[..., None])
        np.save(field_path, result.field)
        replay = None
        if configuration["rung"] == "C0":
            delta = np.abs(result.composite.astype(np.int64) - gff.read_u16(Path(str(retained_c0["composite"]))).astype(np.int64))
            same = np.array_equal(np.rint(result.field).astype(np.uint16), gff.read_zmap(Path(str(retained_c0["zmap"]))))
            if delta.max() != 0 or not same:
                raise AssertionError("index C0 real replay mismatch")
            replay = {"composite_max_sample_difference": 0, "zmap_equal": True,
                      "retained_composite_sha256": retained_c0["composite_sha256"], "retained_zmap_sha256": retained_c0["zmap_sha256"]}
        rows.append({"configuration_id": configuration["id"], "rung": configuration["rung"], "threshold": threshold,
                     "rect_xywh": list(rect), "coverage": coverage_proof, "prepared_manifest_sha256": sha256_path(prepared),
                     "alignment_report_sha256": manifest["alignment_report_sha256"], "raw_inputs": manifest["raw_inputs"],
                     "aligned_inputs": manifest["aligned_inputs"], "composite": str(composite), "zmap": str(zmap), "field": str(field_path),
                     "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap), "field_sha256": sha256_path(field_path),
                     "trusted_pixels": int(result.trusted.sum()), "filled_pixels": int(result.filled.sum()),
                     "unresolved_pixels": int(result.unresolved.sum()), "adjacent_fallback_pixels": int(result.adjacent_fallback.sum()),
                     "dependency_bounds": _index_dependency_bounds(str(configuration["rung"]), 16),
                     "truth_metrics": "not computed", "replay": replay})
    c0 = next((row for row in rows if row["rung"] == "C0"), None)
    montages = []
    if c0 is not None:
        for row in rows:
            if row["rung"] == "A3":
                montage = root / "stack" / f"{row['configuration_id']}-current-vs-candidate.png"
                gff.montage(str(montage), (0, rect[3], 0, rect[2]), [Path(str(c0["composite"])), Path(str(row["composite"]))])
                montages.append({"configuration_id": row["configuration_id"], "path": str(montage), "sha256": sha256_path(montage)})
    return {"source": source, "rect_xywh": list(rect), "prepared_manifest": str(prepared.resolve()),
            "prepared_manifest_sha256": sha256_path(prepared), "alignment_report_sha256": manifest["alignment_report_sha256"],
            "raw_inputs": manifest["raw_inputs"], "aligned_inputs": manifest["aligned_inputs"], "coverage": coverage_proof,
            "input_sha256": {path.name: sha256_path(path) for path in inputs}, "rows": rows, "montages": montages}


def _control_effect(clean: float, value: float, higher_is_better: bool) -> float:
    return max(0.0, clean - value) if higher_is_better else abs(value - clean)


def _calibrated_control(apply: object, measure: object, clean: float, target: float, higher_is_better: bool) -> tuple[np.ndarray, dict[str, object]]:
    """Quantize a control to the requested metric change without hiding a floor."""
    def effect(strength: int) -> tuple[np.ndarray, float]:
        image = apply(strength)
        value = measure(image)
        if value is None:
            raise AssertionError("control made its designated metric unavailable")
        return image, _control_effect(clean, float(value), higher_is_better)
    floor_image, floor = effect(1)
    requested = target
    if target <= floor:
        return floor_image, {"requested_allowance": requested, "strength_counts": 1, "effect": floor,
                             "quantization_floor": floor, "target_representable": target == floor}
    low, high = 1, 1
    image, measured = floor_image, floor
    while measured < target and high < 65535:
        low, high = high, min(65535, high * 2)
        image, measured = effect(high)
    if measured < target:
        raise AssertionError(f"control cannot reach allowance {target}")
    while low + 1 < high:
        middle = (low + high) // 2
        candidate, candidate_effect = effect(middle)
        if candidate_effect >= target:
            high, image, measured = middle, candidate, candidate_effect
        else:
            low = middle
    return image, {"requested_allowance": requested, "strength_counts": high, "effect": measured,
                   "quantization_floor": floor, "target_representable": measured == target}


def run_detector_controls(allowance_path: Path, out: Path | None = None) -> list[dict[str, object]]:
    """Inject five registered defects at the allowance and twice that change."""
    limits = json.loads(allowance_path.read_text())
    def allowance(name: str) -> float:
        values = [float(x["allowance"]) for x in limits if x["metric"] == name]
        return max(values) if values else 0.0
    family = next(x for x in FAMILIES if x.name == "thin-hairs")
    _case, data, _ = prepare_case(allowance_path.parent / "detector-input", family.name, "default", TRAINING_SEEDS[0])
    truth = data.truth.astype(np.uint16)
    masks = np.ones((2, *truth.shape[:2]), dtype=bool)
    masks[0, :, :32] = False
    border_source = gff.ArraySource(np.stack((truth, truth)), masks)
    clean = metrics.measure(truth, truth, data.metadata, border_source, vertical_lines=(256,))
    clean_status = {k: ("perfect" if k == "psnr" and v is None else "unavailable" if v is None else "silent") for k, v in clean.items()}
    designated = ("hair_retention", "edge_halo", "seam_energy", "chroma_drift", "border_contamination")
    if any(clean_status[name] != "silent" for name in designated):
        raise AssertionError("clean detector control fired")
    results = []
    controls = (
        ("blur", "hair_retention", True, lambda strength: np.rint(truth.astype(np.float64) + min(1.0, strength / 65535.0) * (fusion.expand(fusion.reduce(truth.astype(np.float32)), *truth.shape[:2]) - truth)).clip(0, 65535).astype(np.uint16), None),
        ("halo", "edge_halo", False, None, None),
        ("seam", "seam_energy", False, lambda strength: np.concatenate((truth[:, :256], np.clip(truth[:, 256:].astype(np.int64) + strength, 0, 65535).astype(np.uint16)), axis=1), (256,)),
        ("colour", "chroma_drift", False, None, None),
        ("border", "border_contamination", False, lambda strength: np.concatenate((np.clip(truth[:, :32].astype(np.int64) - strength, 0, 65535).astype(np.uint16), truth[:, 32:]), axis=1), None),
    )
    for name, metric, higher_is_better, injector, lines in controls:
        target = allowance(metric)
        for factor in (1.0, 2.0):
            if name == "halo":
                luma = truth.mean(2); edge = np.hypot(*np.gradient(gff.box_mean(luma, 3))) >= gff.GLOW_EDGE
                dist, _ = gff.edge_distance(edge, 64); dark = luma < gff.box_mean(luma, gff.GLOW_BOX)
                def injector(strength: int) -> np.ndarray:
                    image = truth.copy()
                    image[(dist >= 4) & (dist <= 24) & dark] = np.clip(image[(dist >= 4) & (dist <= 24) & dark].astype(np.int64) + strength, 0, 65535).astype(np.uint16)
                    return image
            elif name == "colour":
                def injector(strength: int) -> np.ndarray:
                    image = truth.copy()
                    image[..., 0] = np.clip(image[..., 0].astype(np.int64) + strength, 0, 65535).astype(np.uint16)
                    image[..., 2] = np.clip(image[..., 2].astype(np.int64) - strength, 0, 65535).astype(np.uint16)
                    return image
            source = border_source if name == "border" else None
            vertical = () if lines is None else lines
            measure = lambda image: metrics.measure(image, truth, data.metadata, source, vertical_lines=vertical)[metric]
            image, calibration = _calibrated_control(injector, measure, float(clean[metric]), target * factor, higher_is_better)
            value = measure(image)
            results.append({"defect": name, "metric": metric, "allowance": target, "factor": factor, "value": value,
                             "status": "ok" if calibration["effect"] >= target * factor else "FAIL", **calibration})
    if out is not None:
        _write_json(out, {"clean": clean, "clean_status": clean_status, "controls": results})
    return results


ALIGN_SETTINGS = {
    "algorithm": "pyramidal-ecc",
    "mode": "rotation-scale",
    "reference_policy": "anchor",
    "reference_frame": "middle",
    "backend": "cpu",
}
VALID_ALIGNMENT_STATUSES = {"converged", "stalled", "hit-cap"}


def _real_source_paths(source: str) -> list[Path]:
    """Return the registered original-resolution files for one real source."""
    if source != "stack":
        raise ValueError("real preparation supports --source stack only")
    top = os.environ.get("FOCUSFORGE_REFERENCE_STACK")
    if not top:
        raise ValueError("FOCUSFORGE_REFERENCE_STACK is required for --source stack")
    files = sorted(path for path in Path(top).glob("*.tif") if "Stacked" not in path.name)
    if len(files) != REAL_CROPS["stack"]["frame_count"]:
        raise ValueError(f"FOCUSFORGE_REFERENCE_STACK has {len(files)} frames; expected 61")
    paths = [files[index] for index in REAL_CROPS["stack"]["frame_indices"]]
    expected = (4781, 3136)
    if any((info["width"], info["height"]) != expected for info in map(_header, paths)):
        raise ValueError(f"{source} selected frames do not have registered full-resolution dimensions")
    return paths


def _header(path: Path) -> dict[str, int]:
    info = gff.rawtiff.read_info(path)
    return {"width": int(info.width), "height": int(info.height), "samples_per_pixel": int(info.samples_per_pixel)}


def _raw_bindings(paths: list[Path]) -> list[dict[str, object]]:
    return [{"source_path": str(path.resolve()), "name": path.name, "sha256": sha256_path(path), **_header(path)} for path in paths]


def _selected_input_dir(out: Path, paths: list[Path]) -> Path:
    selected = out / "selected" / "frames"
    selected.mkdir(parents=True, exist_ok=True)
    for index, path in enumerate(paths):
        link = selected / f"{index:02d}.tif"
        if link.exists() or link.is_symlink():
            link.unlink()
        link.symlink_to(path.resolve())
    return selected.parent


def _rss_kb(stderr: str) -> int | None:
    line = next((line for line in stderr.splitlines() if "Maximum resident set size" in line), None)
    return None if line is None else int(line.split()[-1])


def _alignment_command(selected: Path, aligned: Path) -> list[str]:
    return [str(ROOT / "target" / "debug" / "focusforge-cli"), "align", str(selected), "--algo", ALIGN_SETTINGS["algorithm"],
            "--set", f"mode={ALIGN_SETTINGS['mode']}", "--set", f"reference-policy={ALIGN_SETTINGS['reference_policy']}",
            "--set", f"reference-frame={ALIGN_SETTINGS['reference_frame']}", "--no-gpu", "--json", "--write-warped", str(aligned)]


def _report_params(report: dict[str, object]) -> dict[str, object]:
    return {str(row["id"]): row["value"] for row in report.get("params", []) if isinstance(row, dict) and "id" in row and "value" in row}


def _finite_transform(entry: dict[str, object]) -> bool:
    matrix = entry.get("transform", {}).get("m") if isinstance(entry.get("transform"), dict) else None
    if not isinstance(matrix, list) or len(matrix) != 3:
        return False
    try:
        return all(math.isfinite(float(value)) for row in matrix for value in row) and all(len(row) == 3 for row in matrix)
    except (TypeError, ValueError):
        return False


def _validate_preparation(manifest_path: Path, source: str, expected_sources: list[Path] | None = None) -> dict[str, object]:
    """Validate one immutable strict alignment result before it supplies pixels."""
    if not manifest_path.is_file():
        raise ValueError("prepared alignment manifest is required before crop or fusion")
    manifest = json.loads(manifest_path.read_text())
    if manifest.get("source") != source:
        raise ValueError("preparation source does not match --source")
    expected = _real_source_paths(source) if expected_sources is None else expected_sources
    raw = manifest.get("raw_inputs")
    if not isinstance(raw, list) or len(raw) != len(expected):
        raise ValueError("preparation raw input count is incomplete")
    for bound, path in zip(raw, expected):
        if not isinstance(bound, dict) or bound.get("source_path") != str(path.resolve()) or bound.get("sha256") != sha256_path(path) or any(bound.get(key) != value for key, value in _header(path).items()):
            raise ValueError("preparation raw input is stale or does not match the registered source")
    if manifest.get("alignment_exit") != 0 or manifest.get("strict") is not True or manifest.get("settings") != ALIGN_SETTINGS or manifest.get("reference_frame") != 3:
        raise ValueError("preparation did not finish with strict alignment exit 0")
    report_path = Path(str(manifest.get("alignment_report", "")))
    if not report_path.is_file() or manifest.get("alignment_report_sha256") != sha256_path(report_path):
        raise ValueError("preparation alignment report is missing or stale")
    report = json.loads(report_path.read_text())
    params = _report_params(report)
    if report.get("algorithm") != ALIGN_SETTINGS["algorithm"] or report.get("reference_policy") != ALIGN_SETTINGS["reference_policy"] or report.get("reference") != 3 or report.get("failure_policy") != "strict" or report.get("failure_count") != 0 or report.get("align_backend") != ALIGN_SETTINGS["backend"] or report.get("warped_dir") != manifest.get("aligned_dir") or params.get("mode") != ALIGN_SETTINGS["mode"] or params.get("reference-policy") != ALIGN_SETTINGS["reference_policy"] or params.get("reference-frame") != ALIGN_SETTINGS["reference_frame"]:
        raise ValueError("preparation report does not use the registered strict alignment settings")
    entries = report.get("alignments")
    names = [f"{index:02d}.tif" for index in range(len(expected))]
    if report.get("frame_count") != len(expected) or report.get("frames") != names or not isinstance(entries, list) or len(entries) != len(expected):
        raise ValueError("preparation alignment report does not bind every selected frame")
    if manifest.get("alignment_frames") != entries or manifest.get("coverage_coordinate_system") != "aligned-reference" or manifest.get("coverage_transforms") != entries:
        raise ValueError("preparation has a fallback or non-finite transform")
    for index, (name, entry) in enumerate(zip(names, entries)):
        if not isinstance(entry, dict) or entry.get("frame") != index or entry.get("name") != name or entry.get("status") not in VALID_ALIGNMENT_STATUSES or entry.get("fallback") is not False or not _finite_transform(entry):
            raise ValueError("preparation alignment status or frame binding is invalid")
    if report.get("failure_count") != sum(entry["status"] == "diverged" for entry in entries):
        raise ValueError("preparation failure count contradicts frame statuses")
    reference = entries[3]
    if reference["status"] != "converged" or not np.array_equal(np.asarray(reference["transform"]["m"], dtype=np.float64), np.eye(3, dtype=np.float64)):
        raise ValueError("preparation reference frame must be converged with the identity transform")
    aligned = manifest.get("aligned_inputs")
    if not isinstance(aligned, list) or len(aligned) != len(expected):
        raise ValueError("preparation warped TIFF set is incomplete")
    for bound, name, raw_bound in zip(aligned, names, raw):
        if not isinstance(bound, dict):
            raise ValueError("preparation warped TIFF binding is invalid")
        path = Path(str(bound.get("path", "")))
        if bound.get("name") != name or path.parent != Path(str(manifest.get("aligned_dir", ""))) or not path.is_file() or bound.get("sha256") != sha256_path(path) or any(bound.get(key) != raw_bound.get(key) for key in ("width", "height", "samples_per_pixel")) or any(bound.get(key) != value for key, value in _header(path).items()):
            raise ValueError("preparation warped TIFF is missing or stale")
    return manifest


def _roi_coverage(report: dict[str, object], rect: tuple[int, int, int, int], width: int, height: int) -> dict[str, object]:
    """Measure source coverage inside one aligned-reference rectangle."""
    x0, y0, crop_width, crop_height = rect
    if crop_width <= 0 or crop_height <= 0 or x0 < 0 or y0 < 0 or x0 + crop_width > width or y0 + crop_height > height:
        raise ValueError("registered crop rectangle is outside the prepared frame")
    y, x = np.mgrid[y0:y0 + crop_height, x0:x0 + crop_width].astype(np.float64)
    per_frame = []
    common = np.ones((crop_height, crop_width), dtype=bool)
    for entry in report["alignments"]:
        matrix = np.asarray(entry["transform"]["m"], dtype=np.float64)
        denominator = matrix[2, 0] * x + matrix[2, 1] * y + matrix[2, 2]
        with np.errstate(divide="ignore", invalid="ignore"):
            ux = (matrix[0, 0] * x + matrix[0, 1] * y + matrix[0, 2]) / denominator
            uy = (matrix[1, 0] * x + matrix[1, 1] * y + matrix[1, 2]) / denominator
        covered = (denominator > 1e-6) & (ux >= 0) & (ux <= width - 1) & (uy >= 0) & (uy <= height - 1)
        common &= covered
        per_frame.append({"name": entry["name"], "covered_pixels": int(covered.sum()), "total_pixels": int(covered.size)})
    proof = {"rect_xywh": list(rect), "global_origin_xy": [x0, y0], "per_frame": per_frame,
             "common_covered_pixels": int(common.sum()), "total_pixels": int(common.size), "all_sources_cover_roi": bool(common.all())}
    if not proof["all_sources_cover_roi"]:
        raise ValueError("prepared alignment does not cover every pixel of the registered crop")
    return proof


def prepare_real(root: Path, source: str) -> Path:
    """Align one registered real input set and write its validation manifest."""
    root = root.resolve()
    if root.exists() and any(root.iterdir()):
        raise ValueError("--out must be an empty preparation directory")
    paths = _real_source_paths(source)
    raw = _raw_bindings(paths)
    selected = _selected_input_dir(root, paths)
    aligned_dir = root / "aligned"
    aligned_dir.mkdir(parents=True)
    command = _alignment_command(selected, aligned_dir)
    process = subprocess.run(["/usr/bin/time", "-v", *command], capture_output=True, text=True)
    report_path = root / "alignment-report.json"
    try:
        report = json.loads(process.stdout)
    except json.JSONDecodeError:
        report = None
    if isinstance(report, dict):
        _write_json(report_path, report)
    aligned = []
    for index in range(len(paths)):
        path = aligned_dir / f"{index:02d}.tif"
        if path.is_file():
            aligned.append({"name": path.name, "path": str(path.resolve()), "sha256": sha256_path(path), **_header(path)})
    manifest = {"source": source, "created_utc": datetime.now(timezone.utc).isoformat(), "strict": True,
                "producer": provenance(()), "settings": ALIGN_SETTINGS, "reference_frame": 3, "raw_inputs": raw, "selected_input_dir": str(selected.resolve()),
                "alignment_command": command, "timed_alignment_command": ["/usr/bin/time", "-v", *command], "alignment_exit": process.returncode,
                "alignment_stdout": process.stdout, "alignment_stderr": process.stderr, "wall_seconds": report.get("wall_seconds") if isinstance(report, dict) else None,
                "peak_rss_kb": _rss_kb(process.stderr), "alignment_report": str(report_path.resolve()),
                "alignment_report_sha256": sha256_path(report_path) if report_path.is_file() else None,
                "alignment_frames": report.get("alignments") if isinstance(report, dict) else None,
                "coverage_coordinate_system": "aligned-reference", "coverage_transforms": report.get("alignments") if isinstance(report, dict) else None,
                "aligned_dir": str(aligned_dir.resolve()), "aligned_inputs": aligned}
    manifest_path = root / "preparation-manifest.json"
    _write_json(manifest_path, manifest)
    _validate_preparation(manifest_path, source)
    return manifest_path


def _crop_prepared_inputs(root: Path, source: str, manifest_path: Path, rect: tuple[int, int, int, int]) -> tuple[Path, list[Path], dict[str, object]]:
    manifest = _validate_preparation(manifest_path, source)
    report = json.loads(Path(manifest["alignment_report"]).read_text())
    header = manifest["aligned_inputs"][0]
    coverage = _roi_coverage(report, rect, int(header["width"]), int(header["height"]))
    name = "stack-mantis-registered"
    directory = root / "crops" / name / "inputs"
    (directory / "frames").mkdir(parents=True, exist_ok=True)
    x, y, width, height = rect
    outputs = []
    for index, bound in enumerate(manifest["aligned_inputs"]):
        aligned = Path(bound["path"])
        info = gff.rawtiff.read_info(aligned)
        pixels = gff.rawtiff.read_rect(aligned, x, y, width, height, info)
        output = directory / "frames" / f"{index:02d}.tif"
        gff.rawtiff.write_tiff(output, width, height, pixels, bits=16, spp=info.samples_per_pixel)
        outputs.append(output)
    return directory, outputs, coverage


def _crop_montage(root: Path, name: str, rect: tuple[int, int, int, int], rows: list[dict[str, object]]) -> None:
    files = [Path(row["composite"]) for method in METHODS for row in rows if row["case"] == name and row["method"] == method]
    if len(files) != len(METHODS):
        raise AssertionError(f"{name} montage needs one output per method")
    gff.montage(str(root / "crops" / name / "montage.png"), rect, files)


def _registered_rect(source: str, rect_text: str | None) -> tuple[int, int, int, int]:
    if source != "stack":
        raise ValueError("real crops support --source stack only")
    expected = tuple(REAL_CROPS["stack"]["rect_xywh"])
    if rect_text is not None and tuple(int(part) for part in rect_text.split(",")) != expected:
        raise ValueError(f"--rect must equal the registered {source} rectangle {','.join(str(part) for part in expected)}")
    return expected


def run_crops(root: Path, source: str, prepared: Path | None, rect_text: str | None = None) -> list[dict[str, object]]:
    """Fuse one all-source-covered crop from a validated preparation."""
    _real_source_paths(source)
    if prepared is None:
        raise ValueError("--prepared preparation-manifest.json is required before crop or fusion")
    rect = _registered_rect(source, rect_text)
    directory, inputs, coverage = _crop_prepared_inputs(root, source, prepared, rect)
    manifest = _validate_preparation(prepared, source)
    name = "stack-mantis-registered"
    crop_height, crop_width = rect[3], rect[2]
    montage_rect = (0, crop_height, 0, crop_width)
    rows = []
    for method in METHODS:
        work = root / "crops" / name / method
        result = _run_current_baseline(directory, method, 1024, work, _sets(method, False))
        rows.append({"case": name, "method": method, "rect_xywh": list(rect), "montage_rect_y0y1x0x1": montage_rect,
                     "producer": provenance(()),
                     "global_origin_xy": coverage["global_origin_xy"], "coverage": coverage, "frame_indices": REAL_CROPS[source]["frame_indices"],
                     "prepared_manifest": str(prepared.resolve()), "prepared_manifest_sha256": sha256_path(prepared),
                     "alignment_report_sha256": manifest["alignment_report_sha256"], "raw_inputs": manifest["raw_inputs"], "aligned_inputs": manifest["aligned_inputs"],
                     "preparation_command": manifest["alignment_command"], "preparation_exit": manifest["alignment_exit"],
                     "preparation_wall_seconds": manifest["wall_seconds"], "preparation_peak_rss_kb": manifest["peak_rss_kb"],
                     "input_sha256": {path.name: sha256_path(path) for path in inputs}, "frames": len(inputs), "truth_metrics": "not computed",
                     "method_order": list(METHODS), "composite": result["comp"], "zmap": result["zmap"],
                     "composite_sha256": sha256_path(Path(result["comp"])), "zmap_sha256": sha256_path(Path(result["zmap"])), **result})
    _crop_montage(root, name, montage_rect, rows)
    _write_json(root / "crops.json", rows)
    return rows


def preparation_self_check() -> None:
    """Exercise strict preparation validation without invoking alignment."""
    root = ROOT / ".scratch" / "stacking-controls" / f"preparation-self-check-{os.getpid()}"
    saved_stack = os.environ.pop("FOCUSFORGE_REFERENCE_STACK", None)
    try:
        _real_source_paths("stack")
    except ValueError as error:
        if "FOCUSFORGE_REFERENCE_STACK" not in str(error):
            raise
    else:
        raise AssertionError("Stack source was accepted without its environment variable")
    finally:
        if saved_stack is not None:
            os.environ["FOCUSFORGE_REFERENCE_STACK"] = saved_stack
    try:
        _real_source_paths("mantis")
    except ValueError as error:
        if "--source stack only" not in str(error):
            raise
    else:
        raise AssertionError("fixture source was accepted for real preparation")
    shutil.rmtree(root, ignore_errors=True)
    raw_dir, aligned_dir = root / "raw", root / "aligned"
    raw_dir.mkdir(parents=True)
    aligned_dir.mkdir(parents=True)
    raw_paths = []
    for index in range(4):
        image = np.full((4, 4, 3), index, dtype=np.uint16)
        raw = raw_dir / f"raw-{index}.tif"
        aligned = aligned_dir / f"{index:02d}.tif"
        gff.write_u16(raw, image)
        gff.write_u16(aligned, image)
        raw_paths.append(raw)
    report = {"algorithm": "pyramidal-ecc", "reference_policy": "anchor", "reference": 3, "failure_policy": "strict", "failure_count": 0,
              "align_backend": "cpu", "warped_dir": str(aligned_dir), "frame_count": 4, "frames": [f"{index:02d}.tif" for index in range(4)],
              "params": [{"id": "mode", "value": "rotation-scale"}, {"id": "reference-policy", "value": "anchor"}, {"id": "reference-frame", "value": "middle"}],
              "alignments": [{"frame": index, "name": f"{index:02d}.tif", "status": "converged", "fallback": False, "transform": {"m": [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]}} for index in range(4)]}
    report_path = root / "alignment-report.json"
    _write_json(report_path, report)
    manifest_path = root / "preparation-manifest.json"
    def write(exit_code: int = 0) -> None:
        _write_json(manifest_path, {"source": "synthetic", "strict": True, "settings": ALIGN_SETTINGS, "reference_frame": 3, "alignment_exit": exit_code, "raw_inputs": _raw_bindings(raw_paths),
                                    "alignment_report": str(report_path), "alignment_report_sha256": sha256_path(report_path),
                                    "alignment_frames": report["alignments"], "coverage_coordinate_system": "aligned-reference", "coverage_transforms": report["alignments"], "aligned_dir": str(aligned_dir),
                                    "aligned_inputs": [{"name": f"{index:02d}.tif", "path": str(aligned_dir / f"{index:02d}.tif"), "sha256": sha256_path(aligned_dir / f"{index:02d}.tif"), **_header(aligned_dir / f"{index:02d}.tif")} for index in range(4)]})
    write()
    _validate_preparation(manifest_path, "synthetic", raw_paths)
    report["alignments"][0]["status"] = "diverged"
    _write_json(report_path, report)
    write()
    try:
        _validate_preparation(manifest_path, "synthetic", raw_paths)
    except ValueError:
        pass
    else:
        raise AssertionError("diverged alignment status was accepted")
    report["alignments"][0]["status"] = "converged"
    _write_json(report_path, report)
    write()
    report["alignments"][3]["status"] = "stalled"
    _write_json(report_path, report)
    write()
    try:
        _validate_preparation(manifest_path, "synthetic", raw_paths)
    except ValueError:
        pass
    else:
        raise AssertionError("stalled reference status was accepted")
    report["alignments"][3]["status"] = "converged"
    _write_json(report_path, report)
    write()
    for update, restore, message in (
        (lambda: report["alignments"][1].update(status="unknown"), lambda: report["alignments"][1].update(status="converged"), "unknown alignment status was accepted"),
        (lambda: report["alignments"][1].update(frame=7), lambda: report["alignments"][1].update(frame=1), "wrong alignment frame index was accepted"),
        (lambda: report["alignments"][3]["transform"]["m"][0].__setitem__(2, 1.0), lambda: report["alignments"][3]["transform"]["m"][0].__setitem__(2, 0.0), "nonidentity reference transform was accepted"),
        (lambda: report.update(failure_count=1), lambda: report.update(failure_count=0), "contradictory failure count was accepted"),
    ):
        update()
        _write_json(report_path, report)
        write()
        try:
            _validate_preparation(manifest_path, "synthetic", raw_paths)
        except ValueError:
            pass
        else:
            raise AssertionError(message)
        restore()
        _write_json(report_path, report)
        write()
    try:
        _validate_preparation(root / "missing-manifest.json", "synthetic", raw_paths)
    except ValueError:
        pass
    else:
        raise AssertionError("crop input was accepted without preparation")
    for mutator in (lambda: (aligned_dir / "01.tif").unlink(), lambda: gff.write_u16(raw_paths[0], np.full((4, 4, 3), 9, dtype=np.uint16))):
        mutator()
        try:
            _validate_preparation(manifest_path, "synthetic", raw_paths)
        except ValueError:
            pass
        else:
            raise AssertionError("incomplete or stale preparation was accepted")
        shutil.rmtree(root, ignore_errors=True)
        raw_dir.mkdir(parents=True)
        aligned_dir.mkdir(parents=True)
        raw_paths.clear()
        for index in range(4):
            image = np.full((4, 4, 3), index, dtype=np.uint16)
            raw = raw_dir / f"raw-{index}.tif"
            aligned = aligned_dir / f"{index:02d}.tif"
            gff.write_u16(raw, image)
            gff.write_u16(aligned, image)
            raw_paths.append(raw)
        _write_json(report_path, report)
        write()
    write(1)
    try:
        _validate_preparation(manifest_path, "synthetic", raw_paths)
    except ValueError:
        pass
    else:
        raise AssertionError("nonzero alignment exit was accepted")
    write()
    report["failure_policy"] = "lax"
    _write_json(report_path, report)
    write()
    try:
        _validate_preparation(manifest_path, "synthetic", raw_paths)
    except ValueError:
        pass
    else:
        raise AssertionError("bypassed strict alignment was accepted")
    report["failure_policy"] = "strict"
    report["alignments"][1]["transform"]["m"][0][2] = 10.0
    _write_json(report_path, report)
    write()
    manifest = _validate_preparation(manifest_path, "synthetic", raw_paths)
    try:
        _roi_coverage(json.loads(Path(manifest["alignment_report"]).read_text()), (0, 0, 4, 4), 4, 4)
    except ValueError:
        pass
    else:
        raise AssertionError("partial preparation coverage was accepted")
