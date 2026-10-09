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
import shutil
import subprocess
import sys
from typing import Iterable

import numpy as np
import PIL

from . import fusion, metrics
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
    modules = ("__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py")
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
    root = ROOT / ".scratch" / "stacking-controls" / "preparation-self-check"
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
