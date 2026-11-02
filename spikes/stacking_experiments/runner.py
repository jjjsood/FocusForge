# SPDX-License-Identifier: GPL-3.0-or-later
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
import statistics
import subprocess
import sys
import time
from typing import Iterable

import numpy as np
import PIL
from PIL import Image, ImageDraw

from . import coefficient_shrinkage, complex_wavelet, consistency, fusion, index_field, mask_pyramid, metrics, photometric
from .protocol import FAMILIES, FRAME_COUNT, REAL_CROPS, TRAINING_SEEDS, SeedTriple, require_seed
from .scenes import SceneData, family_members, render_family

ROOT = Path(__file__).resolve().parents[2]
GFF_DIR = ROOT / "spikes" / "guided-filter-fusion"
SYNTH_DIR = ROOT / "spikes" / "synthetic-ground-truth"
for directory in (GFF_DIR, SYNTH_DIR):
    if str(directory) not in sys.path:
        sys.path.insert(0, str(directory))
import gff_fusion as gff  # noqa: E402
from synth_stack import blur  # noqa: E402

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


PROVENANCE_MODULES = ("__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py", "focus_score.py", "index_field.py", "consistency.py", "noise_score.py")


def provenance(inputs: Iterable[Path]) -> dict[str, object]:
    """Capture stable local executable and interpreter identifiers."""
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    cli = ROOT / "target" / "debug" / "focusforge-cli"
    cpu_model = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), platform.processor())
    return {"revision": revision, "cli_sha256": sha256_path(cli) if cli.exists() else None,
            "input_sha256": {path.name: sha256_path(path) for path in inputs},
            "runtime": {"python": sys.version.split()[0], "platform": platform.platform(),
                        "numpy": np.__version__, "pillow": PIL.__version__, "cpu_only": True,
                        "cpu_model": cpu_model, "logical_cpu_count": os.cpu_count()},
            "module_sha256": {name: sha256_path(Path(__file__).with_name(name)) for name in PROVENANCE_MODULES},
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


def _case_dir(root: Path, family: str, member: str, seed: SeedTriple, frame_count: int = FRAME_COUNT) -> Path:
    suffix = "" if frame_count == FRAME_COUNT else f"-f{frame_count}"
    return root / "cases" / f"{family}-{member}-l{seed.layout}-t{seed.texture}-n{seed.noise}{suffix}"


def case_split(seed: SeedTriple, allow_holdout: bool = False) -> str:
    """Validate one case seed before any scene data is rendered."""
    return require_seed(seed, allow_holdout)


def prepare_case(root: Path, family_name: str, member: str, seed: SeedTriple, allow_holdout: bool = False,
                 frame_count: int = FRAME_COUNT) -> tuple[Path, SceneData, list[Path]]:
    """Write one deterministic registered input and return its in-memory truth."""
    case_split(seed, allow_holdout)
    family = next(item for item in FAMILIES if item.name == family_name)
    data = render_family(family, seed, member, frame_count)
    case = _case_dir(root, family_name, member, seed, frame_count)
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


def stack_command(case: Path, method: str, tile: int, output: Path, sets: Iterable[str] = (), align: bool = False) -> list[str]:
    """Build the CPU `stack` command line: registered input, no crop, one `--set` per entry.

    `align` runs the product's own rotation-scale alignment instead of skipping it.
    """
    command = [str(ROOT / "target" / "debug" / "focusforge-cli"), "stack", str(case.resolve()), "--algorithm", method]
    command += ["--align-algorithm", "pyramidal-ecc", "--align-set", "mode=rotation-scale"] if align else ["--no-align"]
    command += ["--no-crop", "--cpu", "--json", "--tile-size", str(tile), "--output", str(output.resolve())]
    for entry in sets:
        command += ["--set", entry]
    return command


def photometric_stack_command(case: Path, method: str, tile: int, output: Path, equalized: bool = False) -> list[str]:
    """Build the current CPU stack command with photometric correction disabled."""
    if method not in METHODS or tile != 512:
        raise ValueError("photometric runs require a registered method and 512px tile")
    return stack_command(case, method, tile, output, _sets(method, equalized))


def _photometric_native(case: Path, method: str, output: Path, equalized: bool) -> dict[str, object]:
    """Run stack then current export, retaining each native artifact binding."""
    argv = photometric_stack_command(case, method, 512, output, equalized)
    cli = ROOT / "target" / "debug" / "focusforge-cli"
    time_v, stdout, stderr, native_exit = output / "time-v.txt", output / "stdout.txt", output / "stderr.txt", output / "nativeexit.txt"
    _write_json(output / "argv.json", argv)
    with stdout.open("w") as out, stderr.open("w") as err:
        process = subprocess.run(["/usr/bin/time", "-v", "-o", str(time_v), *argv], cwd=ROOT, stdout=out, stderr=err, text=True)
    native_exit.write_text(f"{process.returncode}\n")
    if process.returncode:
        raise RuntimeError(f"photometric native {method} failed")
    report = json.loads(stdout.read_text())
    project = Path(str(report["outputs"][0]))
    composite, zmap = output / "composite.tif", output / "zmap.tif"
    export = [str(ROOT / "target" / "debug" / "focusforge-cli"), "export", str(project), "-o", str(composite), "--zmap", str(zmap), "--json"]
    export_time, export_stdout, export_stderr, export_exit = output / "export-time-v.txt", output / "export-stdout.txt", output / "export-stderr.txt", output / "exportexit.txt"
    _write_json(output / "export-argv.json", export)
    with export_stdout.open("w") as out, export_stderr.open("w") as err:
        exported = subprocess.run(["/usr/bin/time", "-v", "-o", str(export_time), *export], cwd=ROOT, stdout=out, stderr=err, text=True)
    export_exit.write_text(f"{exported.returncode}\n")
    if exported.returncode:
        raise RuntimeError(f"photometric export {method} failed")
    return {"argv": str((output / "argv.json").resolve()), "argv_sha256": sha256_path(output / "argv.json"),
            "time_v": str(time_v.resolve()), "time_v_sha256": sha256_path(time_v), "stdout": str(stdout.resolve()), "stdout_sha256": sha256_path(stdout),
            "stderr": str(stderr.resolve()), "stderr_sha256": sha256_path(stderr), "native_exit": process.returncode,
            "native_exit_path": str(native_exit.resolve()), "native_exit_sha256": sha256_path(native_exit),
            "export_argv": str((output / "export-argv.json").resolve()), "export_argv_sha256": sha256_path(output / "export-argv.json"),
            "export_time_v": str(export_time.resolve()), "export_time_v_sha256": sha256_path(export_time),
            "export_stdout": str(export_stdout.resolve()), "export_stdout_sha256": sha256_path(export_stdout),
            "export_stderr": str(export_stderr.resolve()), "export_stderr_sha256": sha256_path(export_stderr), "export_exit": exported.returncode,
            "export_exit_path": str(export_exit.resolve()), "export_exit_sha256": sha256_path(export_exit),
            "cli": str(cli.resolve()), "cli_sha256": sha256_path(cli), "runner_sha256": sha256_path(Path(__file__)),
            "project": str(project.resolve()), "project_sha256": sha256_path(project),
            "composite": str(composite.resolve()), "composite_sha256": sha256_path(composite),
            "zmap": str(zmap.resolve()), "zmap_sha256": sha256_path(zmap), "equalization": report.get("equalization")}


def _photometric_case_metrics(case: Path, corrected: Path, native: dict[str, object]) -> tuple[dict[str, object], dict[str, object]]:
    """Measure stored synthetic output against the sealed reference-lighting truth."""
    manifest = json.loads((case / "case.json").read_text())
    metadata = json.loads(Path(str(manifest["metric_metadata"])).read_text())
    truth = gff.read_u16(Path(str(manifest["truth_reference_lighting"])))
    coverage = np.load(manifest["coverage"], allow_pickle=False)
    composite = gff.read_u16(Path(str(native["composite"])))
    zinfo = gff.rawtiff.read_info(Path(str(native["zmap"])))
    zmap = np.frombuffer(gff.rawtiff.read_rect(Path(str(native["zmap"])), 0, 0, zinfo.width, zinfo.height, zinfo), dtype="<u2").reshape(zinfo.height, zinfo.width)
    labels = metrics.label_metrics(zmap, np.clip(np.rint(np.load(manifest["depth"], allow_pickle=False)), 0, 65535).astype(np.uint16), coverage[photometric.REFERENCE])
    values = metrics.measure(composite, truth, metadata)
    measured = {**{name: values[name] for name in ("psnr", "ssim", "edge_halo", "hair_retention", "chroma_drift")}, **labels}
    if any(value is None or not np.isfinite(value) for value in measured.values()):
        raise ValueError("photometric synthetic output omitted a required fused metric")
    fused = photometric.fused_fields({name: float(measured[name]) for name in photometric.REQUIRED_FUSED},
                                     {name: "not applicable to this photometric comparison" for name in photometric.NULL_FUSED})
    frames = np.stack([gff.read_u16(corrected / "frames" / f"{index:02d}.tif") for index in range(FRAME_COUNT)])
    targets = [gff.read_u16(Path(str(row["path"]))) for row in manifest["reference_lighting_frames"]]
    errors = [photometric.frame_error(frame, target, coverage[index] & coverage[photometric.REFERENCE]) for index, (frame, target) in enumerate(zip(frames, targets))]
    return fused, {"frame_errors": errors, "noise": manifest["noise"], "noise_array_sha256": manifest["noise_array_sha256"]}


def _photometric_document(path: Path) -> dict[str, object]:
    try:
        value = json.loads(path.read_text(), object_pairs_hook=photometric._unique_object)
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError("photometric comparison JSON is unreadable") from error
    if not isinstance(value, dict):
        raise ValueError("photometric comparison JSON must be an object")
    return value


def _photometric_finite(value: object) -> bool:
    if isinstance(value, bool) or value is None or isinstance(value, str):
        return True
    if isinstance(value, (int, float)):
        return math.isfinite(value)
    if isinstance(value, list):
        return all(_photometric_finite(item) for item in value)
    if isinstance(value, dict):
        return all(isinstance(key, str) and _photometric_finite(item) for key, item in value.items())
    return False


def _photometric_bound_file(path_value: object, hash_value: object) -> Path:
    path = Path(str(path_value))
    if not isinstance(hash_value, str) or not path.is_file() or sha256_path(path) != hash_value:
        raise ValueError("photometric retained artifact is missing or stale")
    return path


def _photometric_key(row: dict[str, object]) -> tuple[object, ...]:
    return (row.get("variant"), tuple(row.get("seed", ())), row.get("method"), row.get("tile"), row.get("arm"))


def _photometric_row(row: object, expected: dict[str, object], registration_sha256: str) -> dict[str, object]:
    if not isinstance(row, dict) or any(row.get(name) != value for name, value in expected.items()):
        raise ValueError("photometric comparison row key differs from the fixed matrix")
    if row.get("registration_sha256") != registration_sha256 or not _photometric_finite(row):
        raise ValueError("photometric comparison row has stale provenance or nonfinite data")
    source = Path(str(row.get("source_case", "")))
    _photometric_bound_file(source / "case.json", row.get("source_case_sha256"))
    manifest = _photometric_document(source / "case.json")
    if manifest.get("variant") != expected["variant"] or manifest.get("seed") != expected["seed"]:
        raise ValueError("photometric source case differs from its row")
    frames, frame_hashes = manifest.get("frames"), manifest.get("frame_sha256")
    if (not isinstance(frames, list) or not isinstance(frame_hashes, list) or len(frames) != FRAME_COUNT
            or len(frame_hashes) != FRAME_COUNT or any(sha256_path(Path(str(path))) != digest for path, digest in zip(frames, frame_hashes))):
        raise ValueError("photometric source frames are incomplete or stale")
    if row.get("noise_array_sha256") != manifest.get("noise_array_sha256"):
        raise ValueError("photometric row noise binding differs from its source case")
    _photometric_bound_file(manifest.get("noise"), manifest.get("noise_array_sha256"))
    corrected = Path(str(row.get("input_case", "")))
    correction = _photometric_bound_file(corrected / "correction.json", row.get("correction_sha256"))
    correction_document = _photometric_document(correction)
    corrected_hashes = correction_document.get("corrected_sha256")
    if (correction_document.get("arm") != expected["arm"] or correction_document.get("variant") != expected["variant"]
            or correction_document.get("source_frame_sha256") != frame_hashes or not isinstance(corrected_hashes, list)
            or len(corrected_hashes) != FRAME_COUNT
            or any(sha256_path(corrected / "frames" / f"{index:02d}.tif") != digest for index, digest in enumerate(corrected_hashes))):
        raise ValueError("photometric correction inputs are incomplete or stale")
    fits = row.get("fits")
    if expected["arm"] in ("channel-gain-offset", "spatial-multiplier"):
        width = 6 if expected["arm"] == "channel-gain-offset" else 5
        if (fits != correction_document.get("fits") or not isinstance(fits, list) or len(fits) != FRAME_COUNT
                or any(not isinstance(fit, dict) or not isinstance(fit.get("clamped"), bool)
                       or not isinstance(fit.get("applied"), list) or len(fit["applied"]) != width
                       or any(not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(value) for value in fit["applied"])
                       for fit in fits)):
            raise ValueError("photometric fitted parameters are incomplete or stale")
    native = row.get("native")
    if not isinstance(native, dict) or native.get("native_exit") != 0 or native.get("export_exit") != 0:
        raise ValueError("photometric native record has failed")
    for path_name, hash_name in (("project", "project_sha256"), ("composite", "composite_sha256"), ("zmap", "zmap_sha256"),
                                 ("argv", "argv_sha256"), ("time_v", "time_v_sha256"), ("stdout", "stdout_sha256"),
                                 ("stderr", "stderr_sha256"), ("native_exit_path", "native_exit_sha256"),
                                 ("export_argv", "export_argv_sha256"), ("export_time_v", "export_time_v_sha256"),
                                 ("export_stdout", "export_stdout_sha256"), ("export_stderr", "export_stderr_sha256"),
                                 ("export_exit_path", "export_exit_sha256"), ("cli", "cli_sha256")):
        _photometric_bound_file(native.get(path_name), native.get(hash_name))
    if native.get("runner_sha256") != sha256_path(Path(__file__)):
        raise ValueError("photometric native record has a stale runner binding")
    fused = row.get("fused")
    if not isinstance(fused, dict):
        raise ValueError("photometric fused metrics are missing")
    null_metrics = fused.get("null_metrics")
    if (not isinstance(fused.get("metrics"), dict) or not isinstance(null_metrics, dict)
            or set(null_metrics) != set(photometric.NULL_FUSED)
            or any(not isinstance(value, dict) or "value" not in value or value["value"] is not None
                   or not isinstance(value.get("reason"), str) or not value["reason"]
                   for value in null_metrics.values())):
        raise ValueError("photometric unavailable fused metrics are invalid")
    photometric.fused_fields(fused["metrics"], {name: null_metrics[name]["reason"] for name in photometric.NULL_FUSED})
    errors = row.get("frame_errors")
    if (not isinstance(errors, list) or len(errors) != FRAME_COUNT
            or any(not isinstance(value, dict) or any(not isinstance(value.get(name), (int, float)) or isinstance(value[name], bool) or not math.isfinite(value[name])
                                                      for name in ("frame_rgb_mae", "frame_luma_rmse")) for value in errors)):
        raise ValueError("photometric frame errors are missing or nonfinite")
    return row


def _photometric_comparison(document: object, stage: str, registration_sha256: str) -> dict[tuple[object, ...], dict[str, object]]:
    if not isinstance(document, dict) or document.get("stage") != stage or document.get("registration_sha256") != registration_sha256:
        raise ValueError("photometric comparison stage or registration differs")
    rows = document.get("rows")
    expected = photometric.stage_rows(stage)
    if not isinstance(rows, list) or len(rows) != len(expected):
        raise ValueError("photometric comparison row count is incomplete")
    expected_rows = {_photometric_key(row): row for row in expected}
    if len(expected_rows) != len(expected):
        raise ValueError("photometric fixed matrix has duplicate keys")
    matched: dict[tuple[object, ...], dict[str, object]] = {}
    for actual in rows:
        key = _photometric_key(actual) if isinstance(actual, dict) else ()
        required = expected_rows.get(key)
        if required is None:
            raise ValueError("photometric comparison row key differs from the fixed matrix")
        checked = _photometric_row(actual, required, registration_sha256)
        if key in matched:
            raise ValueError("photometric comparison has duplicate fixed keys")
        matched[key] = checked
    expected_keys = set(expected_rows)
    if set(matched) != expected_keys:
        raise ValueError("photometric comparison fixed keys are incomplete")
    return matched


def photometric_repeat_preflight(training: Path, training_sha256: str, registration_sha256: str) -> dict[str, object]:
    """Verify the immutable quality artifact before repeat output can exist."""
    if not isinstance(training_sha256, str) or not training.is_file() or sha256_path(training) != training_sha256:
        raise ValueError("photometric repeat training comparison is missing or stale")
    document = _photometric_document(training)
    _photometric_comparison(document, "quality", registration_sha256)
    return document


def _photometric_semantics(row: dict[str, object]) -> dict[str, object]:
    native = dict(row["native"])
    correction = _photometric_document(Path(str(row["input_case"])) / "correction.json")
    return {"source_case_sha256": row["source_case_sha256"], "noise_array_sha256": row["noise_array_sha256"],
            "correction": {"source_frame_sha256": correction.get("source_frame_sha256"), "coverage_sha256": correction.get("coverage_sha256"),
                           "fits": correction.get("fits"), "corrected_pixels": [_photometric_tiff_pixels(Path(str(row["input_case"])) / "frames" / f"{index:02d}.tif") for index in range(FRAME_COUNT)]},
            "native": {"composite": _photometric_tiff_pixels(Path(str(native["composite"]))), "zmap": _photometric_tiff_pixels(Path(str(native["zmap"]))),
                       "cli_sha256": native["cli_sha256"], "runner_sha256": native["runner_sha256"], "equalization": native["equalization"]},
            "fused": row["fused"], "frame_errors": row["frame_errors"]}


def _photometric_tiff_pixels(path: Path) -> dict[str, object]:
    try:
        info = gff.rawtiff.read_info(path)
        pixels = gff.rawtiff.read_rect(path, 0, 0, info.width, info.height, info)
    except (OSError, ValueError) as error:
        raise ValueError("photometric native TIFF is unreadable") from error
    return {"width": info.width, "height": info.height, "samples_per_pixel": info.samples_per_pixel,
            "bits_per_sample": list(info.bits_per_sample), "pixels_sha256": sha256(pixels).hexdigest()}


def photometric_repeat_equality(training: dict[str, object], repeated: dict[str, object], registration_sha256: str) -> dict[str, object]:
    """Compare repeat artifacts by fixed keys and retained content, never paths."""
    first = _photometric_comparison(training, "quality", registration_sha256)
    try:
        second = _photometric_comparison(repeated, "repeat", registration_sha256)
    except ValueError as error:
        return {"status": "DEFER", "full_matched_keys": False, "training_count": len(first), "repeat_count": 0,
                "counts": {"quality": len(first), "repeat": 0},
                "registration_sha256": registration_sha256, "semantic_differences": [{"reason": str(error)}]}
    first_semantics = {key: _photometric_semantics(row) for key, row in first.items()}
    second_semantics = {key: _photometric_semantics(row) for key, row in second.items()}
    differences = [{"key": list(key), "fields": [name for name in first_semantics[key] if first_semantics[key][name] != second_semantics[key][name]]}
                   for key in sorted(first) if first_semantics[key] != second_semantics[key]]
    metadata_differences = [{"key": list(key), "artifacts": [name for name in ("project_sha256", "composite_sha256", "zmap_sha256")
                                                               if first[key]["native"][name] != second[key]["native"][name]]}
                            for key in sorted(first) if any(first[key]["native"][name] != second[key]["native"][name] for name in ("project_sha256", "composite_sha256", "zmap_sha256"))]
    return {"status": "NO_GO" if differences else "GO", "full_matched_keys": set(first) == set(second),
            "training_count": len(first), "repeat_count": len(second), "counts": {"quality": len(first), "repeat": len(second)}, "registration_sha256": registration_sha256,
            "semantic_differences": differences, "metadata_differences": metadata_differences}


def _photometric_calibration_records(stage: str, rows: list[dict[str, object]]) -> dict[str, object]:
    """Seal one calibration partition without counting method-duplicate frame observations."""
    control = "off" if stage == "calibration-off" else "global-gain"
    fused, errors, frame_allowance_rows, groups, scalars = [], [], [], [], []
    cases: dict[tuple[object, ...], dict[str, object]] = {}
    for row in rows:
        seed = row["seed"]
        for metric, value in row["fused"]["metrics"].items():
            fused.append({"variant": row["variant"], "method": row["method"], "tile": row["tile"], "control": control,
                          "metric": metric, "seed": seed, "repeat": row["repeat"], "value": value})
        cases.setdefault((row["variant"], tuple(seed), row["repeat"]), row)
    for (variant, seed, repeat), row in cases.items():
        for frame, values in enumerate(row["frame_errors"]):
            for metric in ("frame_rgb_mae", "frame_luma_rmse"):
                if values[metric] is None:
                    raise ValueError("photometric calibration frame error is unavailable")
                frame_allowance_rows.append({"variant": variant, "frame": frame, "control": control, "metric": metric,
                                             "seed": list(seed), "repeat": repeat, "value": values[metric], "units": "encoded code counts", "direction": "lower"})
            errors.append({"variant": variant, "frame": frame, "control": control, "seed": list(seed), "repeat": repeat,
                           "frame_rgb_mae": values["frame_rgb_mae"], "frame_luma_rmse": values["frame_luma_rmse"],
                           "units": "encoded code counts", "direction": "lower"})
    if control == "global-gain":
        for (variant, seed, repeat), row in cases.items():
            if variant != "unchanged-focus":
                continue
            source = Path(str(row["source_case"]))
            manifest = json.loads((source / "case.json").read_text())
            frames = np.stack([gff.read_u16(Path(path)) for path in manifest["frames"]])
            coverage = np.load(manifest["coverage"], allow_pickle=False)
            fits = {arm: [photometric.estimate(arm, frames, coverage, frame) for frame in range(FRAME_COUNT)] for arm in photometric.FITTED_ARMS}
            groups.extend(photometric.identity_observations(type("Seed", (), {"layout": seed[0], "texture": seed[1], "noise": seed[2]})(), fits, repeat))
            for group in groups[-len(photometric.FITTED_ARMS) * FRAME_COUNT:]:
                for scalar in group["scalars"]:
                    scalars.append({**{name: group[name] for name in ("seed", "repeat", "arm", "frame")}, **scalar, "direction": "lower"})
    if len(fused) != 210 * len(photometric.REQUIRED_FUSED) or len(errors) != 560:
        raise ValueError("photometric calibration record count is invalid")
    if control == "global-gain" and (len(groups) != 240 or len(scalars) != 640):
        raise ValueError("photometric identity calibration record count is invalid")
    return {"control": control, "fused": fused, "frame_errors": errors, "identity_groups": groups, "identity_scalars": scalars,
            "fused_allowances": photometric.reduce_allowances(fused, ("variant", "method", "tile", "control", "metric")),
            "frame_allowances": photometric.reduce_allowances(frame_allowance_rows, ("variant", "frame", "control", "metric", "units", "direction")),
            "identity_allowances": photometric.reduce_allowances(scalars, ("arm", "frame", "parameter", "units", "direction")) if scalars else []}


def photometric_full_frame_manifest(manifest_path: Path, source: str = "stack") -> dict[str, object]:
    """Bind full-frame aligned TIFFs and transform-derived coverage before any ROI."""
    manifest = _validate_preparation(manifest_path, source)
    inputs = manifest.get("aligned_inputs")
    if not isinstance(inputs, list) or len(inputs) != FRAME_COUNT:
        raise ValueError("photometric real input has no aligned full-frame TIFFs")
    report = json.loads(Path(str(manifest["alignment_report"])).read_text())
    headers = []
    for bound in inputs:
        path = Path(str(bound.get("path", "")))
        info = gff.rawtiff.read_info(path)
        if not path.is_file() or info.width != bound.get("width") or info.height != bound.get("height") or info.samples_per_pixel != 3:
            raise ValueError("photometric aligned TIFF header is stale")
        headers.append({"path": str(path.resolve()), "sha256": sha256_path(path), "width": info.width, "height": info.height})
    if len({(row["width"], row["height"]) for row in headers}) != 1:
        raise ValueError("photometric full-frame inputs differ in dimensions")
    width, height = headers[0]["width"], headers[0]["height"]
    if not isinstance(report.get("alignments"), list) or len(report["alignments"]) != FRAME_COUNT:
        raise ValueError("photometric preparation transforms are incomplete")
    manifest = {**manifest, "photometric_full_frame": {"width": width, "height": height, "frames": headers,
                                                           "coverage": photometric_full_frame_coverage(report, width, height)}}
    return manifest


def photometric_full_frame_coverage(report: dict[str, object], width: int, height: int, tile: int = 512) -> dict[str, object]:
    """Count transform-valid full-frame pixels tilewise; never assume crop coverage."""
    if width <= 0 or height <= 0 or tile <= 0:
        raise ValueError("photometric full-frame geometry is invalid")
    entries = report.get("alignments")
    if not isinstance(entries, list) or len(entries) != FRAME_COUNT:
        raise ValueError("photometric transforms are incomplete")
    totals = [0] * FRAME_COUNT
    common_total = 0
    for y0 in range(0, height, tile):
        for x0 in range(0, width, tile):
            yy, xx = np.mgrid[y0:min(y0 + tile, height), x0:min(x0 + tile, width)].astype(np.float64)
            common = np.ones(xx.shape, dtype=bool)
            for index, entry in enumerate(entries):
                matrix = np.asarray(entry["transform"]["m"], dtype=np.float64)
                denominator = matrix[2, 0] * xx + matrix[2, 1] * yy + matrix[2, 2]
                with np.errstate(divide="ignore", invalid="ignore"):
                    ux = (matrix[0, 0] * xx + matrix[0, 1] * yy + matrix[0, 2]) / denominator
                    uy = (matrix[1, 0] * xx + matrix[1, 1] * yy + matrix[1, 2]) / denominator
                valid = np.isfinite(ux) & np.isfinite(uy) & (denominator > 1e-6) & (ux >= 0) & (ux <= width - 1) & (uy >= 0) & (uy <= height - 1)
                totals[index] += int(valid.sum())
                common &= valid
            common_total += int(common.sum())
    return {"coordinate_system": "aligned-reference", "tile": tile, "per_frame_valid_pixels": totals,
            "common_valid_pixels": common_total, "total_pixels": width * height,
            "transforms": report["alignments"]}


def photometric_real_roi_coverage(full: dict[str, object], rect: tuple[int, int, int, int]) -> dict[str, object]:
    """Require the registered ROI to be wholly covered before real processing."""
    coverage = full.get("coverage")
    if not isinstance(coverage, dict) or not isinstance(coverage.get("transforms"), list):
        raise ValueError("photometric full-frame coverage is incomplete")
    return _roi_coverage({"alignments": coverage["transforms"]}, rect, int(full["width"]), int(full["height"]))


def photometric_stream_apply_full_frame(manifest_path: Path, source: str, arm: str, output: Path) -> dict[str, object]:
    """Fit/apply an arm over aligned TIFF rectangles before extracting the registered ROI."""
    manifest = photometric_full_frame_manifest(manifest_path, source)
    full = manifest["photometric_full_frame"]
    width, height = int(full["width"]), int(full["height"])
    paths = [Path(str(row["path"])) for row in full["frames"]]
    infos = [gff.rawtiff.read_info(path) for path in paths]
    step = max(1, int(math.ceil(math.sqrt(width * height / 262144))))
    samples = [[] for _ in range(FRAME_COUNT)]
    transforms = full["coverage"]["transforms"]
    for y0 in range(0, height, 512):
        for x0 in range(0, width, 512):
            h, w = min(512, height - y0), min(512, width - x0)
            blocks = [np.frombuffer(gff.rawtiff.read_rect(path, x0, y0, w, h, info), dtype="<u2").reshape(h, w, 3) for path, info in zip(paths, infos)]
            yy, xx = np.mgrid[y0:y0+h, x0:x0+w]
            grid = (yy % step == 0) & (xx % step == 0)
            valid = []
            for entry in transforms:
                matrix = np.asarray(entry["transform"]["m"], dtype=np.float64); den = matrix[2,0]*xx + matrix[2,1]*yy + matrix[2,2]
                with np.errstate(divide="ignore", invalid="ignore"):
                    ux=(matrix[0,0]*xx+matrix[0,1]*yy+matrix[0,2])/den; uy=(matrix[1,0]*xx+matrix[1,1]*yy+matrix[1,2])/den
                valid.append(np.isfinite(ux)&np.isfinite(uy)&(den>1e-6)&(ux>=0)&(ux<=width-1)&(uy>=0)&(uy<=height-1))
            reference_luma = blocks[photometric.REFERENCE].mean(axis=2)
            for index, block in enumerate(blocks):
                chosen = grid & valid[index] & valid[photometric.REFERENCE] & (reference_luma >= .05*65535) & (reference_luma <= .95*65535) & (block.mean(axis=2)>0)
                samples[index].append((xx[chosen], yy[chosen], block[chosen], blocks[photometric.REFERENCE][chosen]))
    fits = []
    for index, parts in enumerate(samples):
        xs=np.concatenate([part[0] for part in parts]); ys=np.concatenate([part[1] for part in parts])
        values=np.concatenate([part[2] for part in parts]); targets=np.concatenate([part[3] for part in parts])
        fits.append(photometric.estimate_samples(arm, values, targets, xs, ys, width, height, index))
    output.mkdir(parents=True, exist_ok=True)
    corrected = []
    for index, (path, info, fit) in enumerate(zip(paths, infos, fits)):
        raw = bytearray(width * height * 3 * 2)
        for y0 in range(0, height, 512):
            for x0 in range(0, width, 512):
                h, w = min(512, height-y0), min(512, width-x0)
                block = np.frombuffer(gff.rawtiff.read_rect(path, x0, y0, w, h, info), dtype="<u2").reshape(h, w, 3)
                value = photometric.apply(block, fit, (x0, y0), (height, width)).astype("<u2", copy=False).tobytes()
                for row in range(h):
                    offset = ((y0 + row) * width + x0) * 6
                    raw[offset:offset + w * 6] = value[row*w*6:(row+1)*w*6]
        target = output / f"{index:02d}.tif"
        gff.rawtiff.write_tiff(target, width, height, bytes(raw), bits=16, spp=3)
        output_values = np.frombuffer(raw, dtype="<u2")
        input_pixels = _photometric_tiff_pixels(path)
        output_pixels = _photometric_tiff_pixels(target)
        identity = bool(fit.identity and index == photometric.REFERENCE)
        if identity and input_pixels["pixels_sha256"] != output_pixels["pixels_sha256"]:
            raise ValueError("photometric reference correction is not pixel-identical")
        corrected.append({"path": str(target.resolve()), "sha256": sha256_path(target), "fit": fit.record(),
                          "input_pixels_sha256": input_pixels["pixels_sha256"], "output_pixels_sha256": output_pixels["pixels_sha256"],
                          "reference_identity_proof": identity,
                          "output_saturated_samples": int(((output_values == 0) | (output_values == 65535)).sum()),
                          "output_samples": int(output_values.size)})
    return {"manifest_sha256": sha256_path(manifest_path), "coverage": full["coverage"], "fits": corrected}


def _photometric_crop_after_full_frame(root: Path, frames: list[dict[str, object]], rect: tuple[int, int, int, int]) -> Path:
    """Extract a registered ROI only after a full-frame correction has been written."""
    x, y, width, height = rect
    directory = root / "frames"
    directory.mkdir(parents=True, exist_ok=True)
    for index, binding in enumerate(frames):
        source = Path(str(binding["path"])); info = gff.rawtiff.read_info(source)
        pixels = gff.rawtiff.read_rect(source, x, y, width, height, info)
        gff.rawtiff.write_tiff(directory / f"{index:02d}.tif", width, height, pixels, bits=16, spp=3)
    return root


def _photometric_roi_bindings(case: Path) -> list[dict[str, str]]:
    """Bind the exact corrected ROI TIFFs supplied to one native stack command."""
    bindings = []
    for index in range(FRAME_COUNT):
        path = case / "frames" / f"{index:02d}.tif"
        bindings.append({"path": str(path.resolve()), "sha256": sha256_path(path), "pixels_sha256": _photometric_tiff_pixels(path)["pixels_sha256"]})
    return bindings


def _photometric_real_montages(output: Path, records: list[dict[str, object]], rect: tuple[int, int, int, int], coverage: dict[str, object]) -> list[dict[str, object]]:
    """Render one fixed arm-order diagnostic panel per fusion method."""
    montages = []
    arms = ("off", "global-gain", "channel-gain-offset", "spatial-multiplier")
    for method in METHODS:
        rows = {str(row["arm"]): row for row in records if row["method"] == method}
        if set(rows) != set(arms):
            raise ValueError("photometric real montage needs four arms per method")
        path = output / "montages" / f"{method}-off-vs-candidates.png"
        path.parent.mkdir(exist_ok=True)
        gff.montage(str(path), (0, rect[3], 0, rect[2]), [Path(str(rows[arm]["native"]["composite"])) for arm in arms])
        rendered = Image.open(path).convert("RGB")
        canvas = Image.new("RGB", (rendered.width, rendered.height + 24), "black")
        canvas.paste(rendered, (0, 24))
        ImageDraw.Draw(canvas).text((4, 4), f"{method}: off | global-gain | channel-gain-offset | spatial-multiplier", fill="white")
        canvas.save(path)
        montages.append({"method": method, "path": str(path.resolve()), "sha256": sha256_path(path), "panels": list(arms),
                         "rect_xywh": list(rect), "coverage": coverage, "truth": "unavailable"})
    return montages


def photometric_physical_controls(root: Path) -> list[dict[str, object]]:
    """Exercise raw RGB16 TIFF application and deliberate physical mutation refusals."""
    root.mkdir(parents=True, exist_ok=True)
    frame = np.broadcast_to(np.arange(1088, dtype=np.uint16)[None, :, None] * 31 + np.arange(736, dtype=np.uint16)[:, None, None] * 7,
                            (736, 1088, 3)).copy()
    fits = {"off": photometric._identity("off"),
            "global-gain": photometric.Fit("global-gain", np.array((1.25,)), False, False, 1, False),
            "channel-gain-offset": photometric.Fit("channel-gain-offset", np.array((1.1, 37., .9, -19., 1.02, 9.)), False, False, 2, False),
            "spatial-multiplier": photometric.Fit("spatial-multiplier", np.array((1., .12, .03, -.04, .02)), False, False, 5, False)}
    records: list[dict[str, object]] = []
    for arm, fit in fits.items():
        source = root / f"{arm}-source.tif"; gff.write_u16(source, frame)
        whole = root / f"{arm}-whole.tif"; gff.write_u16(whole, photometric.apply(gff.read_u16(source), fit))
        equal = {}
        for tile in (512, 1024):
            stitched = np.empty_like(frame)
            for y0 in range(0, 736, tile):
                for x0 in range(0, 1088, tile):
                    stitched[y0:min(y0+tile,736), x0:min(x0+tile,1088)] = photometric.apply(frame[y0:min(y0+tile,736), x0:min(x0+tile,1088)], fit, (x0,y0), (736,1088))
            path = root / f"{arm}-tile{tile}.tif"; gff.write_u16(path, stitched)
            equal[str(tile)] = np.array_equal(gff.read_u16(whole), gff.read_u16(path))
        records.append({"name": f"physical-{arm}", "ok": all(equal.values()), "whole_sha256": sha256_path(whole), "tiles": equal})
    wrong_origin = np.empty_like(frame)
    spatial = fits["spatial-multiplier"]
    for y0 in range(0, 736, 512):
        for x0 in range(0, 1088, 512): wrong_origin[y0:min(y0+512,736),x0:min(x0+512,1088)] = photometric.apply(frame[y0:min(y0+512,736),x0:min(x0+512,1088)], spatial)
    samples = np.full((FRAME_COUNT, 50, 50, 3), 10000, dtype=np.uint16); samples[0] = 5000; samples[photometric.REFERENCE] = 6000
    coverage = np.ones((FRAME_COUNT,50,50),bool); coverage[0,:,25:] = False
    covered = photometric.estimate("global-gain", samples, coverage, 0)
    bound_samples = np.full((FRAME_COUNT, 50, 50, 3), 5000, dtype=np.uint16)
    bound_samples[photometric.REFERENCE] = 20000
    bounded = photometric.estimate("global-gain", bound_samples, None, 0)
    wrong_reference = photometric.estimate("global-gain", samples, coverage, 0, reference=2)
    black = samples.copy(); black[0,0,0] = 0; invalid = coverage.copy(); invalid[0,0,0] = False
    black_fit = photometric.estimate("global-gain", black, invalid, 0)
    saturated = photometric.apply(np.array([[[65535, 65535, 65535]]],dtype=np.uint16), fits["global-gain"])
    records.extend([
        {"name":"bad-tile-origin","ok":not np.array_equal(gff.read_u16(root / "spatial-multiplier-whole.tif"), wrong_origin)},
        {"name":"bad-reference-binding","ok":bool(wrong_reference.reference == 2 and wrong_reference.values[0] != covered.values[0])},
        {"name":"coverage-intersection","ok":bool(covered.sample_count == 1250 and black_fit.sample_count == 1249)},
        {"name":"bounds-and-saturation","ok":bool(bounded.values[0] == photometric.HIGH and bounded.clamped and int(saturated[0,0,0]) == 65535)},
        {"name":"q16-tie","ok":int(photometric.apply(np.array([[[2,2,2]]],dtype=np.uint16), fits["global-gain"])[0,0,0]) == 3},
    ])
    _write_json(root / "physical-controls.json", {"records": records})
    if not all(record["ok"] for record in records): raise AssertionError("photometric physical TIFF control failed")
    return records


def _photometric_case_inputs(root: Path) -> dict[str, Path]:
    """Create each fixed synthetic input once, then bind its ordered physical TIFF hashes."""
    root.mkdir(parents=True, exist_ok=True)
    cases: dict[str, Path] = {}
    for variant in photometric.VARIANTS:
        for seed in TRAINING_SEEDS:
            key = f"{variant}-{seed.layout}-{seed.texture}-{seed.noise}"
            case = root / key
            manifest_path = case / "case.json"
            if manifest_path.is_file():
                manifest = json.loads(manifest_path.read_text())
                paths = [Path(str(path)) for path in manifest.get("frames", [])]
                if (manifest.get("variant") != variant or manifest.get("seed") != [seed.layout, seed.texture, seed.noise]
                        or len(paths) != FRAME_COUNT or any(not path.is_file() for path in paths)
                        or manifest.get("frame_sha256") != [sha256_path(path) for path in paths]
                        or any(not isinstance(manifest.get(name), str) or not Path(manifest[name]).is_file()
                               or manifest.get(digest) != sha256_path(Path(manifest[name]))
                               for name, digest in (("coverage", "coverage_sha256"), ("noise", "noise_array_sha256"),
                                                    ("truth_reference_lighting", "truth_reference_lighting_sha256"),
                                                    ("depth", "depth_sha256"), ("metric_metadata", "metric_metadata_sha256")))
                        or not isinstance(manifest.get("reference_lighting_frames"), list)
                        or len(manifest["reference_lighting_frames"]) != FRAME_COUNT
                        or any(not Path(str(row.get("path", ""))).is_file() or row.get("sha256") != sha256_path(Path(str(row["path"]))) for row in manifest["reference_lighting_frames"])):
                    raise ValueError("photometric sealed case is stale or incomplete")
                cases[key] = case
                continue
            if case.exists():
                raise ValueError("photometric case directory exists without a sealed manifest")
            data = photometric.build_case(seed, variant)
            frames = case / "frames"
            frames.mkdir(parents=True)
            paths = []
            for index, image in enumerate(data.frames):
                path = frames / f"{index:02d}.tif"
                gff.write_u16(path, image)
                paths.append(path.resolve())
            coverage_path = case / "coverage.npy"
            np.save(coverage_path, np.asarray(data.metadata["coverage"], dtype=bool))
            noise_path = case / "noise.npy"
            np.save(noise_path, np.asarray(data.metadata["noise"], dtype=np.float64))
            targets = []
            target_dir = case / "reference-lighting"
            target_dir.mkdir()
            for index, image in enumerate(np.asarray(data.metadata["reference_lighting_frames"], dtype=np.uint16)):
                target = target_dir / f"{index:02d}.tif"; gff.write_u16(target, image)
                targets.append({"path": str(target.resolve()), "sha256": sha256_path(target)})
            truth_path = case / "truth-reference.tif"; gff.write_u16(truth_path, data.truth)
            depth_path = case / "depth.npy"; np.save(depth_path, data.depth.astype(np.float64))
            metadata_path = case / "metric-metadata.json"
            _write_json(metadata_path, {name: value for name, value in data.metadata.items()
                                        if name not in {"coverage", "noise", "reference_lighting_frames", "truth_reference_lighting"}})
            manifest = {"variant": variant, "seed": [seed.layout, seed.texture, seed.noise], "reference": photometric.REFERENCE,
                        "frames": [str(path) for path in paths], "frame_sha256": [sha256_path(path) for path in paths],
                        "coverage": str(coverage_path.resolve()), "coverage_sha256": sha256_path(coverage_path),
                        "noise": str(noise_path.resolve()), "noise_array_sha256": sha256_path(noise_path), "noise_sha256": data.metadata["noise_sha256"], "clip_fraction": data.metadata["clip_fraction"], "reference_lighting_frames": targets,
                        "truth_reference_lighting": str(truth_path.resolve()),
                        "depth": str(depth_path.resolve()), "depth_sha256": sha256_path(depth_path),
                        "metric_metadata": str(metadata_path.resolve()), "metric_metadata_sha256": sha256_path(metadata_path),
                        "lighting_order": data.metadata["lighting_order"]}
            manifest["truth_reference_lighting_sha256"] = sha256_path(truth_path)
            _write_json(manifest_path, manifest)
            cases[key] = case
    return cases


def _photometric_corrected_case(root: Path, case: Path, arm: str, variant: str) -> Path:
    """Apply a synthetic arm to physical TIFF inputs before a native fusion command."""
    manifest = json.loads((case / "case.json").read_text())
    frames = np.stack([gff.read_u16(Path(path)).astype(np.uint16) for path in manifest["frames"]])
    coverage = np.load(manifest["coverage"], allow_pickle=False)
    destination = root / f"{case.name}-{arm}" / "frames"
    if destination.parent.exists():
        expected = [destination / f"{index:02d}.tif" for index in range(FRAME_COUNT)]
        binding = destination.parent / "correction.json"
        if binding.is_file() and all(path.is_file() for path in expected):
            saved = json.loads(binding.read_text())
            if (saved.get("arm") == arm and saved.get("variant") == variant and saved.get("source_frame_sha256") == manifest["frame_sha256"]
                    and saved.get("coverage_sha256") == manifest["coverage_sha256"] and saved.get("corrected_sha256") == [sha256_path(path) for path in expected]):
                return destination.parent
        raise ValueError("photometric corrected case output is incomplete")
    destination.mkdir(parents=True)
    fits = []
    for index, frame in enumerate(frames):
        if arm == "off": value = frame
        elif arm == "oracle": value = photometric.oracle(frame, variant, index)
        else:
            fit = photometric.estimate(arm, frames, coverage, index)
            fits.append(fit.record())
            value = photometric.apply(frame, fit)
        gff.write_u16(destination / f"{index:02d}.tif", value)
    _write_json(destination.parent / "correction.json", {"arm": arm, "variant": variant, "source_frame_sha256": manifest["frame_sha256"],
                                                          "coverage_sha256": manifest["coverage_sha256"], "fits": fits,
                                                          "corrected_sha256": [sha256_path(destination / f"{index:02d}.tif") for index in range(FRAME_COUNT)]})
    return destination.parent


def run_photometric_stage(stage: str, output: Path, runs: Path, readiness: dict[str, object],
                          prepared: Path | None = None, source: str | None = None,
                          rect: tuple[int, int, int, int] | None = None) -> list[dict[str, object]]:
    """Execute one admitted fixed matrix; this function never admits itself or refusions old baselines."""
    rows = photometric.stage_rows(stage)
    real_full: dict[str, object] | None = None
    real_roi: dict[str, object] | None = None
    real_preparation: dict[str, object] | None = None
    if stage == "real":
        if source != "stack" or prepared is None or rect is None:
            raise ValueError("photometric real stage needs registered full-frame input")
        real_manifest = photometric_full_frame_manifest(prepared, source)
        real_full = dict(real_manifest["photometric_full_frame"])
        real_roi = photometric_real_roi_coverage(real_full, rect)
        real_preparation = {"manifest": str(prepared.resolve()), "manifest_sha256": sha256_path(prepared),
                            "alignment_report": real_manifest["alignment_report"], "alignment_report_sha256": real_manifest["alignment_report_sha256"],
                            "raw_inputs": real_manifest["raw_inputs"], "aligned_inputs": real_manifest["aligned_inputs"], "reference_frame": real_manifest["reference_frame"]}
    output.mkdir(parents=True, exist_ok=False)
    records: list[dict[str, object]] = []
    if stage == "real":
        assert prepared is not None and source is not None and rect is not None and real_full is not None and real_roi is not None and real_preparation is not None
        full_cache: dict[str, dict[str, object]] = {}
        crop_cache: dict[str, Path] = {}
        for row in rows:
            arm, method = str(row["arm"]), str(row["method"])
            if arm not in full_cache:
                full_cache[arm] = photometric_stream_apply_full_frame(prepared, source, arm, output / "full" / arm)
                crop_cache[arm] = _photometric_crop_after_full_frame(output / "roi" / arm, full_cache[arm]["fits"], rect)
            full, case = full_cache[arm], crop_cache[arm]
            work = output / f"{arm}-{method}"
            work.mkdir(parents=True)
            native = _photometric_native(case, method, work, False)
            null_reasons = {name: "registered real input has no ground-truth or oracle reference" for name in (*photometric.REQUIRED_FUSED, *photometric.NULL_FUSED)}
            null_reasons.update({name: "diagnostic is not registered for this real comparison" for name in ("gradient_energy", "map_coherence")})
            records.append({**row, "registration_sha256": readiness["registration_sha256"], "preparation": real_preparation, "full_frame": full, "roi": real_roi,
                            "input_case": str(case.resolve()), "roi_inputs": _photometric_roi_bindings(case),
                            "native": native, "image_metrics": {name: None for name in null_reasons}, "metric_unavailable": null_reasons,
                            "metric_count": len(null_reasons), "truth_metrics": "not computed"})
        montages = _photometric_real_montages(output, records, rect, real_roi)
        _write_json(output / "real-summary.json", {"source": source, "preparation": real_preparation, "full_frame": real_full,
                                                     "roi": real_roi, "rows": records, "montages": montages,
                                                     "truth": "unavailable", "oracle": "unavailable"})
    else:
        cases = _photometric_case_inputs(runs / "cases")
        for row in rows:
            seed = row["seed"]; key = f"{row['variant']}-{seed[0]}-{seed[1]}-{seed[2]}"; source_case = cases[key]
            source_manifest = _photometric_document(source_case / "case.json")
            arm = "off" if stage == "parity" else ("off" if row.get("control") == "off" else "global-gain" if row.get("control") == "global-gain" else str(row.get("arm", "off")))
            case = source_case if arm == "off" and stage == "parity" else _photometric_corrected_case(output / "inputs", source_case, arm, str(row["variant"]))
            work = output / "native" / f"{key}-{row['method']}-{row.get('repeat', 0)}-{arm}"
            work.mkdir(parents=True)
            native = _photometric_native(case, str(row["method"]), work, stage == "parity")
            correction = case / "correction.json"
            correction_document = _photometric_document(correction) if correction.is_file() else {}
            record = {**row, "registration_sha256": readiness["registration_sha256"], "source_case": str(source_case.resolve()),
                      "source_case_sha256": sha256_path(source_case / "case.json"), "source_frame_sha256": source_manifest["frame_sha256"],
                      "noise_array_sha256": source_manifest["noise_array_sha256"], "input_case": str(case.resolve()),
                      "correction_sha256": sha256_path(correction) if correction.is_file() else None, "fits": correction_document.get("fits"), "native": native}
            if stage == "parity":
                frames = np.stack([gff.read_u16(Path(path)) for path in json.loads((source_case / "case.json").read_text())["frames"]])
                coverage = np.load(json.loads((source_case / "case.json").read_text())["coverage"], allow_pickle=False)
                record["parity"] = photometric.parity_global(frames, coverage, native["equalization"])
            else:
                fused, frame = _photometric_case_metrics(source_case, case, native)
                record.update({"fused": fused, **frame})
            records.append(record)
    payload = {"stage": stage, "registration_sha256": readiness["registration_sha256"], "readiness": readiness, "rows": records}
    _write_json(output / "comparison.json", payload)
    if stage == "parity":
        keys = {(row["variant"], tuple(row["seed"]), row["method"], row["tile"]) for row in records}
        if len(records) != 35 or len(keys) != 35:
            raise ValueError("photometric parity lacks exact unique native comparisons")
        _write_json(output / "parity.json", payload)
    if stage.startswith("calibration"):
        calibration = _photometric_calibration_records(stage, records)
        _write_json(output / "calibration.json", {"registration_sha256": readiness["registration_sha256"], **calibration})
    return records


def run_photometric_source_controls(root: Path) -> list[dict[str, object]]:
    """Run the four registered current-product equalization controls once."""
    if root.exists():
        raise ValueError("photometric source-control output must not already exist")
    root.mkdir(parents=True)
    cases = (("thin999", 999, 20000, 20000), ("usable1000", 1000, 20000, 20000),
             ("clamped", 512 * 512, 5000, 20000), ("positive-half-q16", 512 * 512, 16000, 20000))
    records = []
    for name, usable, source_value, reference_value in cases:
        case, output = root / name / "input", root / name / "output"
        frames = case / "frames"
        frames.mkdir(parents=True, exist_ok=True)
        reference = np.zeros((512, 512, 3), dtype=np.uint16)
        reference.reshape(-1, 3)[:usable] = reference_value
        source = np.full_like(reference, source_value)
        if name == "positive-half-q16":
            reference[0, 0] = 3
            source[0, 0] = 2
        paths = []
        for index in range(FRAME_COUNT):
            path = frames / f"{index:02d}.tif"
            gff.write_u16(path, reference if index == (FRAME_COUNT - 1) // 2 else source)
            paths.append(path)
        output.mkdir(parents=True, exist_ok=True)
        argv = photometric_stack_command(case, "laplacian", 512, output, equalized=True)
        argv_path, stdout, stderr, time_v, exit_path = (root / name / "argv.json", root / name / "stdout.txt", root / name / "stderr.txt", root / name / "time-v.txt", root / name / "exit.txt")
        timed = ["/usr/bin/time", "-v", "-o", str(time_v), *argv]
        argv_path.write_text(json.dumps(timed) + "\n")
        with stdout.open("w") as out_handle, stderr.open("w") as err_handle:
            process = subprocess.run(timed, cwd=ROOT, stdout=out_handle, stderr=err_handle, text=True)
        exit_path.write_text(f"{process.returncode}\n")
        report = json.loads(stdout.read_text()) if process.returncode == 0 else {}
        if not isinstance(report.get("equalization"), dict):
            raise RuntimeError(f"photometric source control {name} omitted equalization")
        equalization = report["equalization"]
        frames_report = equalization.get("frames")
        if equalization.get("reference") != (FRAME_COUNT - 1) // 2 or not isinstance(frames_report, list) or len(frames_report) != FRAME_COUNT:
            raise RuntimeError(f"photometric source control {name} reported an invalid reference")
        others = [entry for index, entry in enumerate(frames_report) if index != (FRAME_COUNT - 1) // 2]
        if name == "thin999":
            valid = all(entry.get("thin") is True and entry.get("samples") == 999 and entry.get("applied") == 1.0 for entry in others)
        elif name == "usable1000":
            valid = all(entry.get("thin") is False and entry.get("samples") == 1000 and entry.get("applied") == 1.0 for entry in others)
        elif name == "clamped":
            valid = all(entry.get("thin") is False and entry.get("samples") == 512 * 512 and entry.get("estimated") == 4.0 and entry.get("applied") == 1.25 and entry.get("clamped") is True for entry in others)
        else:
            valid = all(entry.get("thin") is False and entry.get("samples") == 512 * 512 - 1 and entry.get("estimated") == 1.25 and entry.get("applied") == 1.25 and entry.get("clamped") is False for entry in others)
        if not valid:
            raise RuntimeError(f"photometric source control {name} did not meet its registered outcome")
        records.append({"name": name, "native_exit": process.returncode, "argv_path": str(argv_path), "stdout": str(stdout), "stderr": str(stderr), "time_v": str(time_v), "exit_path": str(exit_path),
                        "frame_sha256": [sha256_path(path) for path in paths]})
        if process.returncode:
            raise RuntimeError(f"photometric source control {name} exited {process.returncode}")
    _write_json(root / "source-controls.json", {"records": records})
    return records


def _run_current_baseline(case: Path, method: str, tile: int, work: Path, sets: tuple[str, ...], align: bool = False) -> dict[str, object]:
    """Run stack and export against the current append-only project format."""
    output = work / f"{method}-t{tile}"
    shutil.rmtree(output, ignore_errors=True)
    output.mkdir(parents=True)
    stack = [str(part) for part in ("/usr/bin/time", "-v", *stack_command(case, method, tile, output, sets, align))]
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
            "equalization": report["equalization"], "alignment": report["align"], "backend": report["backend"]}


def _difference(left: Path, right: Path) -> dict[str, object]:
    a, b = gff.read_u16(left), gff.read_u16(right)
    delta = np.abs(a.astype(np.int64) - b.astype(np.int64))
    return {"sha256_equal": sha256_path(left) == sha256_path(right), "max_abs": int(delta.max()), "different_samples": int((delta != 0).sum())}


def _annotate_differences(rows: list[dict[str, object]]) -> None:
    repeats: dict[tuple[object, ...], list[dict[str, object]]] = defaultdict(list)
    tiles: dict[tuple[object, ...], list[dict[str, object]]] = defaultdict(list)
    for row in rows:
        frames = row.get("frames", FRAME_COUNT)
        repeats[(row["family"], row["member"], frames, row["seed"], row["method"], row["tile"], row["equalized"])].append(row)
        tiles[(row["family"], row["member"], frames, row["seed"], row["method"], row["repeat"], row["equalized"])].append(row)
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
    grouped: dict[tuple[str, str, int, str, int, str], list[dict[str, object]]] = defaultdict(list)
    for row in rows:
        for metric, value in row["metrics"].items():
            if value is not None:
                grouped[(row["family"], row["member"], row.get("frames", FRAME_COUNT), row["method"], row["tile"], metric)].append(row)
    output = []
    for key, records in sorted(grouped.items()):
        by_seed: dict[str, list[float]] = defaultdict(list)
        for row in records:
            by_seed[row["seed"]].append(float(row["metrics"][key[-1]]))
        seed_values = [values[0] for _, values in sorted(by_seed.items())]
        repeat_spread = max((max(values) - min(values) for values in by_seed.values()), default=0.0)
        seed_spread = max(seed_values) - min(seed_values) if seed_values else 0.0
        output.append({"family": key[0], "member": key[1], "frames": key[2], "method": key[3], "tile": key[4], "metric": key[5],
                       "seed_spread": seed_spread, "repeat_spread": repeat_spread, "allowance": max(seed_spread, repeat_spread)})
    return output


Member = tuple[str, str, int]


def baseline_call_count(seeds: Iterable[SeedTriple], members: tuple[Member, ...] | None = None) -> int:
    """Return the exact number of CPU baseline calls for a seed selection.

    `members` lists (family, member, frame count); omitted, every fusion-baseline family member runs at the registered frame count.
    """
    count = len(members) if members is not None else sum(len(family_members(family)) for family in FAMILIES if family.fusion_baseline)
    return len(tuple(seeds)) * count * len(METHODS) * len(TILES) * len(REPEATS)


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


def run_baselines(root: Path, seeds: Iterable[SeedTriple], equalized: bool = False,
                  members: tuple[Member, ...] | None = None) -> list[dict[str, object]]:
    """Run every registered CPU baseline for the supplied non-sealed seeds, over `members` when given."""
    cli = ROOT / "target" / "debug" / "focusforge-cli"
    if not cli.exists():
        raise FileNotFoundError(f"missing executable: {cli}")
    selected = tuple(seeds)
    expected = baseline_call_count(selected, members)
    rows: list[dict[str, object]] = []
    if members is None:
        members = tuple((family.name, member, FRAME_COUNT) for family in FAMILIES if family.fusion_baseline for member in family_members(family))
    for family_name, member, frame_count in members:
        family = next(item for item in FAMILIES if item.name == family_name)
        suffix = "" if frame_count == FRAME_COUNT else f"-f{frame_count}"
        for seed in selected:
            if family.name == "invalid-borders" and member == "aligned-coverage":
                rows.extend(_aligned_coverage_records(root, family, seed, equalized))
                continue
            case, data, inputs = prepare_case(root, family.name, member, seed, frame_count=frame_count)
            source = _source(case)
            identity = provenance(inputs)
            for method in METHODS:
                for tile in TILES:
                    for repeat in REPEATS:
                        work = root / "runs" / run_key(family.name + "-" + member + suffix, seed, method, tile, repeat)
                        result = _run_current_baseline(case, method, tile, work, _sets(method, equalized))
                        composite, zmap = Path(result["comp"]), Path(result["zmap"])
                        boundaries = tile_boundaries(data.truth.shape[1], data.truth.shape[0], tile, int(result["margin"]), int(result["levels"]))
                        row = {"case": str(case), "family": family.name, "member": member, "frames": frame_count, "seed": seed.text(), "method": method, "tile": tile,
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
                eligibility_reach: int | None = None, score_radius: int = 4, adjust: object = None) -> fusion.FusionResult:
    if method == "laplacian":
        return fusion.details(frames, coverage, levels=5, radius=8, score=score, adjust=adjust)
    if method == "weighted-average":
        return fusion.weighted_average(frames, coverage, radius=16, score=score, adjust=adjust)
    if method == "depth-map":
        return fusion.depth_map_replay(frames, coverage, score_radius=score_radius, smooth_radius=8,
                                       eligibility_reach=eligibility_reach, score=score, adjust=adjust)
    raise ValueError(f"unknown fusion method: {method}")


def _tile_edges(length: int, boundaries: list[object]) -> list[int]:
    return [0, *(int(value) for value in boundaries), length]


def _tiled_focus(frames: np.ndarray, coverage: np.ndarray | None, method: str, vertical: list[object],
                 horizontal: list[object], margin: int, score: object = None,
                 eligibility_reach: int | None = None, score_radius: int = 4, adjust: object = None) -> fusion.FusionResult:
    """Fuse recorded interiors from halo-expanded tiles."""
    if not vertical and not horizontal:
        return _focus_fuse(frames, coverage, method, score, eligibility_reach, score_radius, adjust)
    height, width = frames.shape[1:3]
    composite = np.zeros_like(frames[0], dtype=np.uint16)
    winners = np.full((height, width), fusion.NO_SOURCE, dtype=np.uint16)
    eligible = np.zeros((frames.shape[0], height, width), dtype=bool)
    for y0, y1 in zip(_tile_edges(height, horizontal), _tile_edges(height, horizontal)[1:]):
        for x0, x1 in zip(_tile_edges(width, vertical), _tile_edges(width, vertical)[1:]):
            outer_y0, outer_y1 = max(0, y0 - margin), min(height, y1 + margin)
            outer_x0, outer_x1 = max(0, x0 - margin), min(width, x1 + margin)
            crop_coverage = None if coverage is None else coverage[:, outer_y0:outer_y1, outer_x0:outer_x1]
            crop = _focus_fuse(frames[:, outer_y0:outer_y1, outer_x0:outer_x1], crop_coverage, method, score, eligibility_reach, score_radius, adjust)
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
                 score_radius: int = 4, return_plan: bool = False, adjust: object = None) -> fusion.FusionResult | tuple[fusion.FusionResult, dict[str, object]]:
    """Run one registered method with a halo sufficient for the selected score and mask."""
    frames, coverage = _focus_inputs(row)
    plan = focus_tile_plan(row, frames.shape[1], frames.shape[2], eligibility_reach, score_radius)
    result = _tiled_focus(frames, coverage, str(row["method"]), list(plan["vertical"]), list(plan["horizontal"]),
                          int(plan["margin"]), score, eligibility_reach, score_radius, adjust)
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


def replay_focus_baselines(root: Path, expected: int = 180) -> list[dict[str, object]]:
    """Require exact current-score images and source maps before candidate rows."""
    rows = json.loads((root / "baseline.json").read_text())
    selected = [row for row in rows if row["repeat"] == 0]
    if len(selected) != expected:
        raise ValueError(f"expected {expected} baseline replay rows, got {len(selected)}")
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
                             budget: dict[str, object], configure: object = focus_configuration) -> list[dict[str, object]]:
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
            result, candidate_plan = configure(row, configuration, return_plan=True)
            frame_count = int(row.get("frames", FRAME_COUNT))
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
            suffix = "" if frame_count == FRAME_COUNT else f"-f{frame_count}"
            directory = output / str(configuration["id"]) / run_key(f"{row['family']}-{row['member']}{suffix}", SeedTriple.parse(str(row["seed"])), str(row["method"]), int(row["tile"]), 0)
            directory.mkdir(parents=True, exist_ok=True)
            composite, zmap = directory / "composite.tif", directory / "zmap.tif"
            gff.write_u16(composite, result.composite)
            gff.write_u16(zmap, result.winners[..., None])
            family = next(item for item in FAMILIES if item.name == row["family"])
            data = render_family(family, SeedTriple.parse(str(row["seed"])), str(row["member"]), frame_count)
            metric_row = {**row, "comp": str(composite), "zmap": str(zmap), "tile_boundaries": candidate_plan}
            metric_row.pop("tile_difference", None)
            source = gff.TiffSource(row["actual_source_frames"], row["actual_coverage"]) if "actual_source_frames" in row else _source(Path(str(row["case"])))
            image_metrics, record_metadata = _record_metrics(data, str(row["family"]), str(row["member"]), metric_row,
                                                               composite, zmap, source, candidate_plan)
            label = {name: image_metrics.pop(name) for name in ("label_exact", "label_within_one", "label_mad")}
            records.append({"configuration_id": configuration["id"], "matched_control": configuration.get("matched_control"),
                            "classification": configuration.get("classification", "control"), "can_advance": configuration.get("can_advance", False),
                            "family": row["family"], "member": row["member"], "frames": frame_count, "seed": row["seed"], "method": row["method"], "tile": row["tile"],
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
                                               and item.get("frames", FRAME_COUNT) == frame_count
                                               and item["method"] == row["method"] and item["tile"] == row["tile"]]})
    groups: dict[tuple[object, ...], list[dict[str, object]]] = defaultdict(list)
    for record in records:
        groups[(record["configuration_id"], record["family"], record["member"], record["frames"], record["seed"], record["method"])].append(record)
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


def _noise_allowance(allowances: list[dict[str, object]], row: dict[str, object]) -> float | None:
    """Return the frozen flat-noise match tolerance for one retained key."""
    matches = [item for item in allowances if item.get("family") == row["family"] and item.get("member") == row["member"]
               and item.get("method") == "laplacian" and item.get("tile") == row["tile"] and item.get("metric") == "flat_noise"]
    return None if len(matches) != 1 else float(matches[0]["allowance"])


def _noise_record(output: Path, row: dict[str, object], identifier: str, kind: str, result: coefficient_shrinkage.ShrinkResult,
                  image: np.ndarray, source: object, data: SceneData, counters: dict[str, int], match: dict[str, object] | None = None) -> dict[str, object]:
    """Persist one shrinkage output with immutable selection hashes and shared metrics."""
    directory = output / identifier / run_key(f"{row['family']}-{row['member']}", SeedTriple.parse(str(row["seed"])), "laplacian", int(row["tile"]), int(row["repeat"]))
    directory.mkdir(parents=True, exist_ok=True)
    composite, zmap = directory / "composite.tif", directory / "zmap.tif"
    gff.write_u16(composite, image)
    gff.write_u16(zmap, result.zmap[..., None])
    metric_row = {**row, "comp": str(composite), "zmap": str(zmap), "tile_boundaries": row["tile_boundaries"]}
    metric_row.pop("tile_difference", None)
    image_metrics, metadata = _record_metrics(data, str(row["family"]), str(row["member"]), metric_row, composite, zmap, source, row["tile_boundaries"])
    metadata["metric_unavailable"]["tile_difference"] = "training output fused whole-input; no paired tile comparison"
    return {"configuration_id": identifier, "classification": kind, "family": row["family"], "member": row["member"],
            "seed": row["seed"], "tile": row["tile"], "repeat": row["repeat"], "composite": str(composite), "zmap": str(zmap),
            "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap),
            "raw_winner_sha256": [sha256(part.tobytes()).hexdigest() for part in result.raw_winners],
            "final_winner_sha256": [sha256(part.tobytes()).hexdigest() for part in result.final_winners],
            "residual_sha256": sha256(result.residual.tobytes()).hexdigest(),
            "thresholds": list(result.thresholds), "stream_reads": counters,
            "image_metrics": image_metrics, "unavailable_metrics": metadata["metric_unavailable"], "match": match}


def run_noise_shrinkage_configurations(root: Path, output: Path, allowances_path: Path,
                                       multipliers: tuple[int, ...] = (1, 2, 4, 8), family_filter: str | None = None) -> list[dict[str, object]]:
    """Evaluate registered shrinkage and cached finite controls for retained training inputs."""
    baseline = json.loads((root / "baseline.json").read_text())
    rows = [row for row in baseline if row["method"] == "laplacian" and row["repeat"] == 0
            and (family_filter is None or row["family"] == family_filter)]
    expected = 60 if family_filter is None else 20
    if len(rows) != expected:
        raise AssertionError(f"noise-shrinkage input count is {len(rows)}, expected {expected}")
    allowances = json.loads(allowances_path.read_text())
    records: list[dict[str, object]] = []
    gaussian_strengths = (0.25, 0.5, 0.75, 1.0, 1.5, 2.0)
    attenuation_strengths = (0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875)
    rendered: dict[tuple[str, str, str], SceneData] = {}
    for row in rows:
        source = _selection_source(row)
        key = (str(row["family"]), str(row["member"]), str(row["seed"]))
        data = rendered.get(key)
        if data is None:
            family = next(item for item in FAMILIES if item.name == row["family"])
            original = render_family(family, SeedTriple.parse(str(row["seed"])), str(row["member"]))
            data = SceneData(np.empty((0,), dtype=np.uint16), original.truth, original.depth, original.metadata)
            del original
            rendered[key] = data
        identity, identity_counts = coefficient_shrinkage.fuse_streaming(source, 0.0)
        retained = gff.read_u16(Path(str(row["composite"])))
        if not np.array_equal(identity.composite, retained) or not np.array_equal(identity.zmap, gff.read_zmap(Path(str(row["zmap"])))):
            raise AssertionError("threshold-zero shrinkage does not replay retained Details")
        identity_record = _noise_record(output, row, "shrink-identity", "control", identity, identity.composite, source, data, identity_counts)
        records.append(identity_record)
        candidates: dict[int, tuple[coefficient_shrinkage.ShrinkResult, dict[str, object]]] = {}
        for multiplier in multipliers:
            result, result_counts = coefficient_shrinkage.fuse_streaming(source, float(multiplier))
            if (any(not np.array_equal(left, right) for left, right in zip(identity.raw_winners, result.raw_winners))
                    or any(not np.array_equal(left, right) for left, right in zip(identity.final_winners, result.final_winners))
                    or not np.array_equal(identity.residual, result.residual)):
                raise AssertionError("shrinkage changed selection or residual")
            candidates[multiplier] = (result, _noise_record(output, row, f"shrink-m{multiplier}", "candidate", result, result.composite, source, data, result_counts))
            records.append(candidates[multiplier][1])
        def flat(image: np.ndarray) -> float | None:
            if row["family"] != "flat-noise":
                return None
            return metrics.measure(image, data.truth, data.metadata, source, tuple(row["tile_boundaries"]["vertical"]), tuple(row["tile_boundaries"]["horizontal"])).get("flat_noise")
        gaussian_trials = [(strength, flat(fusion.quantize_u16(blur(identity.composite.astype(np.float64), strength)))) for strength in gaussian_strengths]
        attenuation_trials: list[tuple[float, coefficient_shrinkage.ShrinkResult, dict[str, int], float | None]] = []
        for strength in attenuation_strengths:
            result, result_counts = coefficient_shrinkage.fuse_streaming(source, 0.0, attenuation=strength)
            attenuation_trials.append((strength, result, result_counts, flat(result.composite)))
        tolerance = _noise_allowance(allowances, row)
        for multiplier, (candidate, record) in candidates.items():
            target = record["image_metrics"].get("flat_noise")
            gaussian = coefficient_shrinkage.match_strength(target if isinstance(target, (float, int)) else None, gaussian_trials)
            attenuated = coefficient_shrinkage.match_strength(target if isinstance(target, (float, int)) else None, [(strength, value) for strength, _result, _counts, value in attenuation_trials])
            for control_id, selected, trials in (("gaussian", gaussian, gaussian_trials), ("attenuation", attenuated, [(strength, value) for strength, _result, _counts, value in attenuation_trials])):
                match = {"target": target, "trials": [{"strength": strength, "flat_noise": value} for strength, value in trials], "allowance": tolerance}
                if selected is None or tolerance is None or selected[2] > tolerance:
                    match.update({"status": "unmatched", "selected": None if selected is None else {"strength": selected[0], "flat_noise": selected[1], "error": selected[2]}})
                    records.append({"configuration_id": f"shrink-m{multiplier}-{control_id}", "classification": "control",
                                    "family": row["family"], "member": row["member"], "seed": row["seed"], "tile": row["tile"],
                                    "repeat": row["repeat"], "composite": None, "zmap": None, "match": match,
                                    "raw_winner_sha256": [sha256(part.tobytes()).hexdigest() for part in candidate.raw_winners],
                                    "final_winner_sha256": [sha256(part.tobytes()).hexdigest() for part in candidate.final_winners],
                                    "residual_sha256": sha256(candidate.residual.tobytes()).hexdigest(), "stream_reads": record["stream_reads"]})
                    continue
                match.update({"status": "matched", "selected": {"strength": selected[0], "flat_noise": selected[1], "error": selected[2]}})
                if control_id == "gaussian":
                    image = fusion.quantize_u16(blur(identity.composite.astype(np.float64), selected[0]))
                    control = identity
                else:
                    control, control_counts, _value = next((result, counts, value) for strength, result, counts, value in attenuation_trials if strength == selected[0])
                    image = control.composite
                if control_id == "gaussian":
                    control_counts = identity_counts
                records.append(_noise_record(output, row, f"shrink-m{multiplier}-{control_id}", "control", control, image, source, data, control_counts, match))
    return records


def noise_shrinkage_weighted_references(root: Path, family_filter: str | None = None) -> list[dict[str, object]]:
    """Reuse frozen Weighted Average rows beside shrinkage outputs."""
    rows = [row for row in json.loads((root / "baseline.json").read_text()) if row["method"] == "weighted-average"
            and row["repeat"] == 0 and (family_filter is None or row["family"] == family_filter)]
    expected = 60 if family_filter is None else 20
    if len(rows) != expected:
        raise AssertionError(f"Weighted Average reference count is {len(rows)}, expected {expected}")
    return rows


def _real_noise_stat(image: np.ndarray) -> float | None:
    """Measure a robust high-pass luma spread without synthetic truth."""
    luma = image.astype(np.float64).mean(2)
    high_pass = luma - blur(luma[..., None], 1.0)[..., 0]
    if not np.isfinite(high_pass).all():
        return None
    return float(np.median(np.abs(high_pass - np.median(high_pass))) / 0.6745)


def run_noise_shrinkage_stack(output: Path, source_name: str, prepared: Path, rect_text: str | None) -> dict[str, object]:
    """Render aligned real shrinkage context with diagnostic-only control matching."""
    rect = _registered_rect(source_name, rect_text)
    manifest = _validate_preparation(prepared, source_name)
    directory, paths, coverage = _crop_prepared_inputs(output, source_name, prepared, rect)
    source = gff.TiffSource(paths)
    identity, _counts = coefficient_shrinkage.fuse_streaming(source, 0.0)
    retained_path = prepared.parents[3] / "stacking-crops" / source_name / "crops.json"
    retained = json.loads(retained_path.read_text()) if retained_path.is_file() else []
    current = next((row for row in retained if row.get("method") == "laplacian" and row.get("prepared_manifest_sha256") == sha256_path(prepared)
                    and row.get("rect_xywh") == list(rect)), None)
    if current is None or not np.array_equal(identity.composite, gff.read_u16(Path(str(current["composite"])))) or not np.array_equal(identity.zmap, gff.read_zmap(Path(str(current["zmap"])))):
        raise ValueError("real threshold-zero replay does not match retained current Details crop")
    gaussian_strengths, attenuation_strengths = (0.25, 0.5, 0.75, 1.0, 1.5, 2.0), (0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875)
    gaussian_trials = [(strength, _real_noise_stat(fusion.quantize_u16(blur(identity.composite.astype(np.float64), strength)))) for strength in gaussian_strengths]
    attenuation_trials = [(strength, *coefficient_shrinkage.fuse_streaming(source, 0.0, attenuation=strength)) for strength in attenuation_strengths]
    rows: list[dict[str, object]] = []
    def record(identifier: str, classification: str, result: coefficient_shrinkage.ShrinkResult, image: np.ndarray,
               diagnostic: dict[str, object] | None = None) -> dict[str, object]:
        work = directory / identifier
        work.mkdir(parents=True, exist_ok=True)
        composite, zmap = work / "composite.tif", work / "zmap.tif"
        gff.write_u16(composite, image)
        gff.write_u16(zmap, result.zmap[..., None])
        return {"configuration_id": identifier, "classification": classification, "rect_xywh": list(rect), "global_origin_xy": coverage["global_origin_xy"],
                "coverage": coverage, "prepared_manifest_sha256": sha256_path(prepared), "alignment_report_sha256": manifest["alignment_report_sha256"],
                "raw_inputs": manifest["raw_inputs"], "aligned_inputs": manifest["aligned_inputs"], "composite": str(composite), "zmap": str(zmap),
                "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap), "raw_winner_sha256": [sha256(item.tobytes()).hexdigest() for item in result.raw_winners],
                "final_winner_sha256": [sha256(item.tobytes()).hexdigest() for item in result.final_winners], "residual_sha256": sha256(result.residual.tobytes()).hexdigest(), "thresholds": list(result.thresholds),
                "real_noise_diagnostic": diagnostic, "truth_metrics": "not computed"}
    rows.append(record("shrink-identity", "control", identity, identity.composite, {"status": "diagnostic-only", "value": _real_noise_stat(identity.composite)}))
    montages = []
    for multiplier in (1, 2, 4, 8):
        candidate, _counts = coefficient_shrinkage.fuse_streaming(source, float(multiplier))
        target = _real_noise_stat(candidate.composite)
        gaussian = coefficient_shrinkage.match_strength(target, gaussian_trials)
        attenuation = coefficient_shrinkage.match_strength(target, [(strength, _real_noise_stat(result.composite)) for strength, result, _counts in attenuation_trials])
        if gaussian is None or attenuation is None:
            raise ValueError("real diagnostic support is unavailable")
        candidate_row = record(f"shrink-m{multiplier}", "candidate", candidate, candidate.composite, {"status": "diagnostic-only", "value": target})
        gaussian_image = fusion.quantize_u16(blur(identity.composite.astype(np.float64), gaussian[0]))
        gaussian_row = record(f"shrink-m{multiplier}-gaussian", "control", identity, gaussian_image,
                              {"status": "diagnostic-only", "target": target, "strength": gaussian[0], "value": gaussian[1], "absolute_error": gaussian[2]})
        attenuation_result = next(result for strength, result, _counts in attenuation_trials if strength == attenuation[0])
        attenuation_row = record(f"shrink-m{multiplier}-attenuation", "control", attenuation_result, attenuation_result.composite,
                                 {"status": "diagnostic-only", "target": target, "strength": attenuation[0], "value": attenuation[1], "absolute_error": attenuation[2]})
        rows.extend((candidate_row, gaussian_row, attenuation_row))
        montage = output / "stack" / f"shrink-m{multiplier}-baseline-candidate-controls.png"
        montage.parent.mkdir(parents=True, exist_ok=True)
        gff.montage(str(montage), (0, source.h, 0, source.w), [Path(rows[0]["composite"]), Path(candidate_row["composite"]), Path(gaussian_row["composite"]), Path(attenuation_row["composite"])])
        montages.append(str(montage))
    return {"rows": rows, "montages": montages}


def noise_shrinkage_resource_controls(root: Path) -> list[dict[str, object]]:
    """Measure file-backed stream reads for registered frame-count controls."""
    rows = [row for row in json.loads((root / "baseline.json").read_text())
            if row["family"] == "flat-noise" and row["member"] == "noise-250" and row["method"] == "laplacian"
            and row["tile"] == 512 and row["repeat"] == 0]
    if len(rows) != 5:
        raise ValueError("resource control needs five retained flat-noise inputs")
    paths = [Path(path) for path in gff.frame_paths(Path(str(rows[0]["case"])))]
    if len(paths) != 8:
        raise ValueError("resource control needs eight file-backed frames")
    records = []
    for count in (2, 8, 16):
        selected = (paths * ((count + len(paths) - 1) // len(paths)))[:count]
        source = gff.TiffSource(selected)
        result, counters = coefficient_shrinkage.fuse_streaming(source, 1.0)
        records.append({"frame_count": count, "physical_input_files": len({str(path) for path in selected}),
                        "sample_reads": counters["sample_reads"], "mask_reads": counters["total_mask_reads"],
                        "source_reads": list(source.reads), "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                        "composite_sha256": sha256(result.composite.tobytes()).hexdigest(), "thresholds": list(result.thresholds)})
    return records


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


def _mask_pyramid_difference(whole: mask_pyramid.MaskPyramidResult,
                             tiled: mask_pyramid.MaskPyramidResult) -> dict[str, int]:
    """Report the registered whole/tile image and finest-label comparison."""
    image = np.abs(whole.composite.astype(np.int64) - tiled.composite.astype(np.int64))
    return {"max_abs": int(image.max()), "different_samples": int(np.count_nonzero(image)),
            "zmap_different_pixels": int(np.count_nonzero(whole.zmap != tiled.zmap))}


def _mask_pyramid_record(output: Path, row: dict[str, object], configuration: dict[str, object],
                         whole: mask_pyramid.MaskPyramidResult, tiled: mask_pyramid.MaskPyramidResult,
                         counters: dict[str, int], tile_counters: dict[str, int], plan: dict[str, object],
                         allowances: list[dict[str, object]], repeat_kind: str) -> dict[str, object]:
    """Write one actual whole/tile pair and its synthetic quality measurements."""
    directory = output / str(configuration["id"]) / run_key(f"{row['family']}-{row['member']}",
                                                                  SeedTriple.parse(str(row["seed"])), "mask-pyramid",
                                                                  int(row["tile"]), int(row["repeat"]))
    directory.mkdir(parents=True, exist_ok=True)
    whole_composite, whole_zmap = directory / "whole-composite.tif", directory / "whole-zmap.tif"
    composite, zmap = directory / "composite.tif", directory / "zmap.tif"
    for path, image in ((whole_composite, whole.composite), (whole_zmap, whole.zmap[..., None]),
                        (composite, tiled.composite), (zmap, tiled.zmap[..., None])):
        gff.write_u16(path, image)
    difference = _mask_pyramid_difference(whole, tiled)
    if difference["max_abs"] > 1 or difference["zmap_different_pixels"] != 0:
        raise AssertionError(f"mask-pyramid whole/tile parity failed for {configuration['id']}")
    source = _selection_source(row)
    family = next(item for item in FAMILIES if item.name == row["family"])
    original = render_family(family, SeedTriple.parse(str(row["seed"])), str(row["member"]))
    data = SceneData(np.empty((0,), dtype=np.uint16), original.truth, original.depth, original.metadata)
    metric_row = {**row, "comp": str(composite), "zmap": str(zmap), "tile_boundaries": plan, "tile_difference": difference}
    image_metrics, metadata = _record_metrics(data, str(row["family"]), str(row["member"]), metric_row,
                                               composite, zmap, source, plan)
    labels = gff.read_zmap(Path(str(row["zmap"])))
    label_agreement = {"exact_pixels": int(np.count_nonzero(tiled.zmap == labels)),
                       "different_pixels": int(np.count_nonzero(tiled.zmap != labels)),
                       "retained_zmap_sha256": row["zmap_sha256"]}
    record: dict[str, object] = {
        "configuration_id": configuration["id"], "classification": configuration["classification"],
        "can_advance": configuration["can_advance"], "mask_rule": configuration["mask_rule"],
        "family": row["family"], "member": row["member"], "seed": row["seed"], "tile": row["tile"],
        "repeat": row["repeat"], "repeat_kind": repeat_kind, "method": "laplacian",
        "whole_composite": str(whole_composite), "whole_zmap": str(whole_zmap),
        "composite": str(composite), "zmap": str(zmap),
        "whole_composite_sha256": sha256_path(whole_composite), "whole_zmap_sha256": sha256_path(whole_zmap),
        "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap),
        "input_identity": row.get("actual_source_sha256", row.get("input_sha256")),
        "selected_input_identity": row.get("actual_source_sha256", row.get("input_sha256")),
        "raw_input_identity": row.get("input_sha256"), "baseline_composite_sha256": row.get("composite_sha256"),
        "baseline_zmap_sha256": row.get("zmap_sha256"), "label_agreement": label_agreement,
        "tile_difference": difference, "tile_plan": plan, "stream_reads": counters,
        "tile_stream_reads": tile_counters, "uncovered": list(tiled.uncovered), "fallbacks": list(tiled.fallbacks),
        "finest_map_sha256": sha256(tiled.zmap.tobytes()).hexdigest(), "whole_level_diagnostics": whole.level_diagnostics,
        "tile_window_diagnostics": [window["level_diagnostics"] for window in plan["windows"]],
        "tile_window_accounting": "read halos overlap; per-window support and fallback totals are not global counts",
        "partition_bound": tiled.partition_bound, "allowance_keys": [item for item in allowances
            if item["family"] == row["family"] and item["member"] == row["member"]
            and item["method"] == "laplacian" and item["tile"] == row["tile"]],
        "image_metrics": image_metrics, "unavailable_metrics": metadata["metric_unavailable"],
        "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
    }
    record["label_metrics"] = {key: record["image_metrics"].pop(key) for key in ("label_exact", "label_within_one", "label_mad")}
    unavailable = record["unavailable_metrics"]
    unavailable.update({"geometry_error": "synthetic fusion inputs have no recovered alignment transform", "alignment_drift": "synthetic fusion inputs have no recovered alignment transform", "alignment_failures": "synthetic fusion inputs have no recovered alignment transform", "gradient_energy": "diagnostic not measured by this arm", "map_coherence": "diagnostic not measured by this arm"})
    record["metric_schema"] = {**record["image_metrics"], **record["label_metrics"],
                               "geometry_error": None, "alignment_drift": None, "alignment_failures": None}
    reasons = {"seam_energy": "no interior tile boundary for this tile plan",
               "label_exact": "no valid label-truth support", "label_within_one": "no valid label-truth support",
               "label_mad": "no valid label-truth support", "gradient_energy": "diagnostic is not measured by this arm",
               "map_coherence": "diagnostic is not measured by this arm"}
    for metric, value in record["metric_schema"].items():
        if value is None:
            unavailable.setdefault(metric, reasons.get(metric, "metric is unavailable for this generated region"))
        elif isinstance(value, (float, np.floating)) and not np.isfinite(value):
            unavailable[metric] = "metric is non-finite for this generated region"
    record["metric_values_sha256"] = sha256(json.dumps(_json(record["metric_schema"]), sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return {**record, **metadata}


def _mask_pyramid_label_audit(row: dict[str, object]) -> dict[str, object]:
    """Classify retained CLI level-zero differences with the frozen f32 order bound."""
    paths = row.get("actual_source_frames") or gff.frame_paths(Path(str(row["case"])))
    frames = np.stack([gff.read_u16(path) for path in paths])
    coverage = np.load(str(row["actual_coverage"])) if "actual_coverage" in row else None
    view = fusion.details_level0(frames, coverage)
    alternate = np.stack([fusion.score_plane_f32_alternate(detail, 8) for detail in view.details])
    tolerance = float(np.max(np.abs(alternate.astype(np.float64) - view.scores.astype(np.float64))))
    retained = gff.read_zmap(Path(str(row["zmap"])))
    disagreements = _twin_disagreements(retained, view.winners, view.scores, view.eligible, tolerance)
    source = _selection_source(row)
    return {"python_exact": bool(np.array_equal(view.winners, mask_pyramid.finest_winners(source))),
            "python_retained_different_pixels": int(np.count_nonzero(view.winners != retained)),
            "cli_twin_disagreements": disagreements, "f32_summation_order_tolerance": tolerance,
            "retained_zmap_sha256": row["zmap_sha256"]}


def run_mask_pyramid_configurations(root: Path, output: Path, configurations: list[dict[str, object]],
                                    allowances_path: Path, repeat_kind: str = "training") -> list[dict[str, object]]:
    """Run the fixed three-arm matrix from retained Details inputs."""
    rows = [row for row in json.loads((root / "baseline.json").read_text())
            if row["method"] == "laplacian" and row["repeat"] == 0]
    if len(rows) != 60:
        raise AssertionError(f"mask-pyramid input count is {len(rows)}, expected 60")
    allowances = json.loads(allowances_path.read_text())
    records = []
    for row in rows:
        source = _selection_source(row)
        label_audit = _mask_pyramid_label_audit(row)
        if not label_audit["python_exact"]:
            raise AssertionError("Python finest winner map does not match the retained Details oracle")
        for configuration in configurations:
            measurement_started = time.monotonic()
            whole, counters = mask_pyramid.fuse_streaming(source, str(configuration["mask_rule"]))
            tiled, tile_counters, plan = mask_pyramid.fuse_tiled(source, str(configuration["mask_rule"]), int(row["tile"]), 160)
            record = _mask_pyramid_record(output, row, configuration, whole, tiled, counters, tile_counters,
                                          plan, allowances, repeat_kind)
            record["old_oracle_label_audit"] = label_audit
            record["measurement_seconds"] = time.monotonic() - measurement_started
            records.append(record)
    if len(records) != 180:
        raise AssertionError("mask-pyramid output count does not match registered matrix")
    return records


def mask_pyramid_references(root: Path) -> dict[str, list[dict[str, object]]]:
    """Return each retained baseline context row used beside the three arm rows."""
    baseline = json.loads((root / "baseline.json").read_text())
    references = {}
    for method in ("laplacian", "weighted-average", "depth-map"):
        rows = [row for row in baseline if row["method"] == method and row["repeat"] == 0]
        if len(rows) != 60:
            raise AssertionError(f"mask-pyramid {method} reference count is {len(rows)}, expected 60")
        references[method] = rows
    return references


def mask_pyramid_gff_context(root: Path, output: Path) -> list[dict[str, object]]:
    """Measure paper-scale GFF only after every retained source binding is preflighted."""
    rows = [row for row in json.loads((root / "baseline.json").read_text())
            if row["method"] == "laplacian" and row["repeat"] == 0]
    if len(rows) != 60:
        raise AssertionError("mask-pyramid GFF context needs 60 retained Details inputs")
    bindings = []
    for row in rows:
        actual = "actual_source_frames" in row
        raw_paths = [Path(path) for path in gff.frame_paths(Path(str(row["case"])))]
        raw_fingerprints = {path.name: sha256_path(path) for path in raw_paths}
        paths = [Path(path) for path in row["actual_source_frames"]] if actual else raw_paths
        fingerprints = {path.name: sha256_path(path) for path in paths}
        expected = row.get("actual_source_sha256") if actual else row.get("input_sha256")
        if (len(raw_paths) != 8 or raw_fingerprints != row.get("input_sha256")
                or len(paths) != 8 or len(fingerprints) != len(paths) or fingerprints != expected
                or row.get("equalized") is not False or "equalize=false" not in row.get("parameters", [])
                or not isinstance(row.get("tile_boundaries"), dict)
                or not Path(str(row.get("zmap", ""))).is_file()
                or row.get("zmap_sha256") != sha256_path(Path(str(row["zmap"])) )):
            raise ValueError("GFF context retained Details binding is missing or stale")
        source = _selection_source(row)
        if not all(isinstance(getattr(source, name, None), int) and getattr(source, name) > 0 for name in ("h", "w", "c")):
            raise ValueError("GFF context source geometry is invalid")
        boundaries = row["tile_boundaries"]
        if (boundaries.get("levels") != 5 or boundaries.get("margin") != 160
                or any(not isinstance(line, int) or not 0 < line < source.w for line in boundaries.get("vertical", []))
                or any(not isinstance(line, int) or not 0 < line < source.h for line in boundaries.get("horizontal", []))):
            raise ValueError("GFF context tile geometry does not match the retained Details input")
        family = next(item for item in FAMILIES if item.name == row["family"])
        rendered = render_family(family, SeedTriple.parse(str(row["seed"])), str(row["member"]))
        data = SceneData(np.empty((0,), dtype=np.uint16), rendered.truth, rendered.depth, rendered.metadata)
        if data.truth.shape != (source.h, source.w, source.c):
            raise ValueError("GFF context truth geometry does not match the retained Details input")
        coverage_file_sha, coverage_mask_sha, alignment = None, None, None
        if actual:
            coverage_path = Path(str(row.get("actual_coverage", "")))
            if (not coverage_path.is_file() or row.get("actual_coverage_sha256") != sha256_path(coverage_path)
                    or not isinstance(row.get("alignment"), dict) or not isinstance(row["alignment"].get("alignments"), list)
                    or len(row["alignment"]["alignments"]) != len(paths) or not isinstance(row["alignment"].get("reference"), int)
                    or not isinstance(row.get("actual_frame_export_commands"), list)
                    or len(row["actual_frame_export_commands"]) != len(paths)):
                raise ValueError("GFF context aligned source or coverage binding is missing or stale")
            alignment_entries, reference = row["alignment"]["alignments"], row["alignment"]["reference"]
            if not 0 <= reference < len(paths):
                raise ValueError("GFF context aligned reference is invalid")
            for index, (path, entry) in enumerate(zip(paths, alignment_entries)):
                matrix = np.asarray(entry.get("transform", {}).get("m"), dtype=np.float64)
                if (not isinstance(entry, dict) or entry.get("frame") != index or entry.get("name") != path.name
                        or entry.get("status") != "converged" or entry.get("fallback") is not False
                        or matrix.shape != (3, 3) or not np.isfinite(matrix).all()):
                    raise ValueError("GFF context aligned transforms are invalid")
            if not np.array_equal(np.asarray(alignment_entries[reference]["transform"]["m"], dtype=np.float64), np.eye(3, dtype=np.float64)):
                raise ValueError("GFF context aligned reference transform is invalid")
            coverage = np.load(str(coverage_path))
            if coverage.shape != (len(paths), source.h, source.w):
                raise ValueError("GFF context coverage geometry does not match exported inputs")
            coverage_file_sha = row["actual_coverage_sha256"]
            coverage_mask_sha = sha256(np.asarray(coverage, dtype=bool).any(axis=0).tobytes()).hexdigest()
            alignment = row["alignment"]
        else:
            coverage_mask_sha = sha256(np.ones((source.h, source.w), dtype=bool).tobytes()).hexdigest()
        bindings.append({"row": row, "source": source, "paths": paths, "fingerprints": fingerprints, "raw_fingerprints": raw_fingerprints,
                         "actual": actual, "coverage_file_sha256": coverage_file_sha,
                         "coverage_mask_sha256": coverage_mask_sha, "alignment": alignment,
                         "data": data, "boundaries": boundaries})
    records = []
    for binding in bindings:
        row, source = binding["row"], binding["source"]
        started = time.monotonic()
        image, covered = gff.fuse_stack(source, scale=1.0)
        covered_sha = sha256(np.asarray(covered, dtype=bool).tobytes()).hexdigest()
        if covered_sha != binding["coverage_mask_sha256"]:
            raise ValueError("GFF context coverage result does not match the retained source")
        directory = output / "gff-context" / run_key(f"{row['family']}-{row['member']}", SeedTriple.parse(str(row["seed"])), "gff", int(row["tile"]), 0)
        directory.mkdir(parents=True, exist_ok=True)
        composite = directory / "composite.tif"
        gff.write_u16(composite, image)
        metric_row = {**row, "comp": str(composite), "zmap": row["zmap"], "tile_boundaries": binding["boundaries"]}
        metric_row.pop("tile_difference", None)
        values, metadata = _record_metrics(binding["data"], str(row["family"]), str(row["member"]), metric_row,
                                           composite, Path(str(row["zmap"])), source, binding["boundaries"])
        for key in ("label_exact", "label_within_one", "label_mad"):
            values.pop(key, None)
        unavailable = metadata["metric_unavailable"]
        unavailable.update({"tile_difference": "guided-filter context has no paired tiled output",
                            "geometry_error": "guided-filter context does not recover an alignment transform",
                            "alignment_drift": "guided-filter context does not recover an alignment transform",
                            "alignment_failures": "guided-filter context does not recover an alignment transform",
                            "gradient_energy": "diagnostic is not measured by this context",
                            "map_coherence": "guided-filter context has no source-index map"})
        for metric, value in values.items():
            if value is None:
                unavailable.setdefault(metric, "metric is unavailable for this retained family region")
            elif isinstance(value, (float, np.floating)) and not np.isfinite(value):
                unavailable[metric] = "metric is non-finite for this retained family region"
        records.append({"family": row["family"], "member": row["member"], "seed": row["seed"], "tile": row["tile"],
                        "context_class": "paper-parameter diagnostic", "settings": {"scale": 1.0, "equalize": False},
                        "source_kind": "exported-aligned" if binding["actual"] else "retained-raw", "input_order": [str(path) for path in binding["paths"]],
                        "input_identity": binding["fingerprints"], "selected_input_identity": binding["fingerprints"],
                        "raw_input_identity": binding["raw_fingerprints"], "actual_source_sha256": row.get("actual_source_sha256"),
                        "actual_coverage_sha256": binding["coverage_file_sha256"], "coverage_mask_sha256": covered_sha,
                        "input_shape_hwc": [source.h, source.w, source.c], "tile_boundaries": binding["boundaries"],
                        "alignment": binding["alignment"], "matched_details_zmap_sha256": row["zmap_sha256"],
                        "composite": str(composite), "composite_sha256": sha256_path(composite), "image_metrics": values,
                        "image_metric_values_sha256": sha256(json.dumps(_json(values), sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
                        "unavailable_metrics": unavailable, "label_metrics": None,
                        "label_metrics_reason": "guided-filter context has no source-index map", "measurement_seconds": time.monotonic() - started})
    return records



def mask_pyramid_resource_body(root: Path, frame_count: int) -> dict[str, object]:
    """Measure one fresh file-backed body without creating an N-frame sample array."""
    if frame_count not in (2, 8, 16, 32):
        raise ValueError("resource body frame count is not registered")
    row = next((item for item in json.loads((root / "baseline.json").read_text())
                if item["family"] == "flat-noise" and item["member"] == "noise-250" and item["method"] == "laplacian"
                and item["tile"] == 512 and item["repeat"] == 0), None)
    if row is None:
        raise ValueError("resource body needs a retained flat-noise Details input")
    physical = [Path(path) for path in gff.frame_paths(Path(str(row["case"])))]
    if len(physical) != 8:
        raise ValueError("resource body needs eight physical input files")
    paths = (physical * ((frame_count + len(physical) - 1) // len(physical)))[:frame_count]
    source = gff.TiffSource(paths)
    result, counters = mask_pyramid.fuse_streaming(source)
    fingerprints = {path.name: sha256_path(path) for path in physical}
    module_sha = sha256_path(Path(__file__).with_name("mask_pyramid.py"))
    return {"frame_count": frame_count, "process_id": os.getpid(), "physical_input_pool_files": len(physical),
            "distinct_referenced_files": len({str(path) for path in paths}),
            "declared_references": [str(path) for path in paths], "sample_reads": counters["sample_reads"],
            "mask_reads": counters["mask_reads"], "generated_default_masks": counters["mask_defaults_generated"],
            "composite_sha256": sha256(result.composite.tobytes()).hexdigest(), "input_fingerprints": fingerprints,
            "source_module_sha256": module_sha, "peak_rss_kb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
            "cache_limitation": "OS page cache is uncontrolled; this is not a cold/warm cache claim"}


_MASK_PYRAMID_RESOURCE_SCRIPT = """
import json
import sys
from pathlib import Path
from spikes.stacking_experiments.runner import mask_pyramid_resource_body
print(json.dumps(mask_pyramid_resource_body(Path(sys.argv[1]), int(sys.argv[2])), sort_keys=True))
"""


def mask_pyramid_resource_argv(root: Path, frame_count: int, time_path: Path) -> list[str]:
    """Return the registered fresh-process invocation without mutable call-site text."""
    return ["/usr/bin/time", "-v", "-o", str(time_path), sys.executable, "-c", _MASK_PYRAMID_RESOURCE_SCRIPT,
            str(root), str(frame_count)]


def mask_pyramid_resource_controls(root: Path, output: Path) -> dict[str, object]:
    """Collect twelve independent file-backed bodies with retained raw evidence."""
    records = []
    for frame_count in (2, 8, 16, 32):
        for repeat in range(3):
            body = output / f"body-n{frame_count}-r{repeat}"
            body.mkdir(parents=True, exist_ok=True)
            stdout_path, stderr_path, time_path, exit_path = body / "stdout.txt", body / "stderr.txt", body / "time-v.txt", body / "exit.txt"
            argv = mask_pyramid_resource_argv(root, frame_count, time_path)
            argv_path = body / "argv.txt"
            argv_path.write_text(json.dumps(argv) + "\n")
            started = time.monotonic()
            with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
                process = subprocess.run(argv, cwd=ROOT, env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}, stdout=stdout, stderr=stderr, text=True)
            elapsed = time.monotonic() - started
            exit_path.write_text(f"{process.returncode}\n")
            stdout_text, stderr_text, time_text = stdout_path.read_text(), stderr_path.read_text(), time_path.read_text() if time_path.is_file() else ""
            if process.returncode:
                raise RuntimeError(f"mask-pyramid resource body exit {process.returncode}: {stderr_text[-1000:]}")
            try:
                record = json.loads(stdout_text)
            except json.JSONDecodeError as error:
                raise RuntimeError("mask-pyramid resource body did not return JSON") from error
            if not isinstance(record, dict):
                raise RuntimeError("mask-pyramid resource body JSON is not an object")
            record.update({"repeat": repeat, "argv": argv, "native_exit": process.returncode, "body_wall_seconds": elapsed,
                           "stdout": str(stdout_path), "stderr": str(stderr_path), "time_v": str(time_path), "exit_path": str(exit_path),
                           "argv_path": str(argv_path), "argv_sha256": sha256(argv_path.read_bytes()).hexdigest(),
                           "stdout_sha256": sha256(stdout_text.encode()).hexdigest(), "stderr_sha256": sha256(stderr_text.encode()).hexdigest(),
                           "time_v_sha256": sha256(time_text.encode()).hexdigest(), "exit_sha256": sha256(exit_path.read_bytes()).hexdigest()})
            records.append(record)
    n2 = [int(record["peak_rss_kb"]) for record in records if record["frame_count"] == 2]
    noise = max(n2) - min(n2)
    medians = {str(count): statistics.median([int(record["peak_rss_kb"]) for record in records if record["frame_count"] == count]) for count in (2, 8, 16, 32)}
    n2_median = medians["2"]
    comparisons = {count: {"median_rss_kb": median, "difference_from_n2_kb": abs(median - n2_median), "within_n2_noise": abs(median - n2_median) <= noise} for count, median in medians.items()}
    return {"records": records, "n2_noise_kb": noise, "median_rss_kb": medians, "comparison": comparisons,
            "resource_guard_completed": len(records) == 12, "resource_flatness_pass": all(item["within_n2_noise"] for item in comparisons.values()),
            "cache_limitation": "OS page cache is uncontrolled; this is not a cold/warm cache claim"}



def run_mask_pyramid_stack(output: Path, source_name: str, prepared: Path, rect_text: str | None,
                           configurations: list[dict[str, object]]) -> dict[str, object]:
    """Render three registered diagnostics after retained real references are verified."""
    rect = _registered_rect(source_name, rect_text)
    manifest = _validate_preparation(prepared, source_name)
    prepared_sha = sha256_path(prepared)
    retained_path = prepared.parents[3] / "stacking-crops" / source_name / "crops.json"
    if not retained_path.is_file():
        raise ValueError("retained prepared-crop references are required before mask-pyramid output")
    retained = json.loads(retained_path.read_text())
    if not isinstance(retained, list):
        raise ValueError("retained prepared-crop reference record is invalid")
    def reference(method: str) -> dict[str, object]:
        row = next((item for item in retained if isinstance(item, dict) and item.get("method") == method
                    and item.get("prepared_manifest_sha256") == prepared_sha
                    and item.get("alignment_report_sha256") == manifest["alignment_report_sha256"]
                    and item.get("rect_xywh") == list(rect)), None)
        if row is None:
            raise ValueError(f"retained {method} crop does not match the preparation")
        if (row.get("raw_inputs") != manifest["raw_inputs"] or row.get("aligned_inputs") != manifest["aligned_inputs"]
                or row.get("coverage", {}).get("rect_xywh") != list(rect)
                or row.get("coverage", {}).get("all_sources_cover_roi") is not True
                or row.get("equalization") is not None or row.get("method_order") != list(METHODS)):
            raise ValueError(f"retained {method} crop settings, source order, or coverage are invalid")
        for field in ("composite", "zmap"):
            path = Path(str(row.get(field, "")))
            if not path.is_file() or row.get(f"{field}_sha256") != sha256_path(path):
                raise ValueError(f"retained {method} crop file is missing or stale")
        return row
    details, weighted = reference("laplacian"), reference("weighted-average")
    directory, paths, coverage = _crop_prepared_inputs(output, source_name, prepared, rect)
    source = gff.TiffSource(paths)
    rows = []
    for configuration in configurations:
        arm_started = time.monotonic()
        result, counters = mask_pyramid.fuse_streaming(source, str(configuration["mask_rule"]))
        work = directory / str(configuration["id"])
        work.mkdir(parents=True, exist_ok=True)
        composite, zmap = work / "composite.tif", work / "zmap.tif"
        gff.write_u16(composite, result.composite)
        gff.write_u16(zmap, result.zmap[..., None])
        rows.append({"configuration_id": configuration["id"], "classification": configuration["classification"], "can_advance": configuration["can_advance"], "mask_rule": configuration["mask_rule"],
                     "rect_xywh": list(rect), "global_origin_xy": coverage["global_origin_xy"], "coverage": coverage,
                     "prepared_manifest_sha256": prepared_sha, "alignment_report_sha256": manifest["alignment_report_sha256"],
                     "raw_inputs": manifest["raw_inputs"], "aligned_inputs": manifest["aligned_inputs"],
                     "composite": str(composite), "zmap": str(zmap), "composite_sha256": sha256_path(composite), "zmap_sha256": sha256_path(zmap),
                     "stream_reads": counters, "uncovered": list(result.uncovered), "fallbacks": list(result.fallbacks),
                     "finest_map_sha256": sha256(result.zmap.tobytes()).hexdigest(), "level_diagnostics": result.level_diagnostics,
                     "reference_inputs": {"details_composite_sha256": details["composite_sha256"], "details_zmap_sha256": details["zmap_sha256"], "weighted_composite_sha256": weighted["composite_sha256"], "weighted_zmap_sha256": weighted["zmap_sha256"]},
                     "measurement_seconds": time.monotonic() - arm_started, "truth_metrics": "not computed"})
    if len(rows) != 3:
        raise AssertionError("real mask-pyramid output count does not match registration")
    candidate = next(row for row in rows if row["configuration_id"] == "soft-coarse")
    montage = output / "stack" / "details-candidate-weighted.png"
    montage.parent.mkdir(parents=True, exist_ok=True)
    retained_labels = gff.read_zmap(Path(str(details["zmap"])))
    for row in rows:
        if not np.array_equal(gff.read_zmap(Path(str(row["zmap"]))), retained_labels):
            raise AssertionError("real mask-pyramid finest map does not match retained Details crop")
        row["retained_details_zmap_sha256"] = details["zmap_sha256"]
    montage_command = [str(getattr(gff, "montage").__module__), "details,candidate,weighted"]
    gff.montage(str(montage), (0, rect[3], 0, rect[2]), [Path(details["composite"]), Path(candidate["composite"]), Path(weighted["composite"])])
    montage_record: dict[str, object] = {"path": str(montage), "sha256": sha256_path(montage), "command": montage_command,
                                         "inputs": [details["composite_sha256"], candidate["composite_sha256"], weighted["composite_sha256"]]}
    gff_started = time.monotonic()
    gff_image, gff_covered = gff.fuse_stack(source, scale=1.0)
    gff_composite = directory / "gff-scale-1.0.tif"
    gff.write_u16(gff_composite, gff_image)
    gff_context = {"scale": 1.0, "input_matches_prepared_crop": True, "composite": str(gff_composite), "composite_sha256": sha256_path(gff_composite),
                   "covered_sha256": sha256(np.asarray(gff_covered, dtype=bool).tobytes()).hexdigest(), "settings": {"scale": 1.0, "equalize": False},
                   "input_identity": {path.name: sha256_path(path) for path in paths}, "input_shape_hwc": [source.h, source.w, source.c],
                   "label_metrics": None, "label_metrics_reason": "guided-filter context has no source-index map", "measurement_seconds": time.monotonic() - gff_started}
    return {"source": source_name, "rect_xywh": list(rect), "prepared_manifest": str(prepared.resolve()), "prepared_manifest_sha256": prepared_sha,
            "alignment_report_sha256": manifest["alignment_report_sha256"], "coverage": coverage, "rows": rows, "montage": montage_record, "gff_context": gff_context}



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


def _wavelet_crop_prepared_inputs(root: Path, source: str, manifest_path: Path, rect: tuple[int, int, int, int]) -> tuple[Path, list[Path], dict[str, object]]:
    """Copy the validated registered crop with wavelet-local retained common masks."""
    manifest = _validate_preparation(manifest_path, source)
    report = json.loads(Path(manifest["alignment_report"]).read_text())
    header = manifest["aligned_inputs"][0]
    coverage = _roi_coverage(report, rect, int(header["width"]), int(header["height"]))
    directory = root / "complex-wavelet-crops" / "stack-mantis-registered" / "inputs"
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
    mask_path = directory / "coverage-masks.npy"
    np.save(mask_path, np.ones((len(outputs), height, width), dtype=bool))
    return directory, outputs, {**coverage, "mask_path": str(mask_path), "mask_sha256": sha256_path(mask_path),
                                "mask_shape": [len(outputs), height, width], "mask_semantics": "validated common aligned crop coverage"}


def _wavelet_maps(root: Path, result: complex_wavelet.FusionResult) -> dict[str, object]:
    """Write separate coefficient and pixel source maps for one wavelet result."""
    root.mkdir(parents=True, exist_ok=True)
    orientation = []
    raw = []
    support = []
    for level, bands in enumerate(result.orientation_winners):
        level_paths, level_raw, level_support = [], [], []
        for band, values in enumerate(bands):
            path = root / f"orientation-l{level}-b{band}.tif"
            gff.write_u16(path, values[..., None])
            level_paths.append({"path": str(path), "sha256": sha256_path(path)})
            raw_path = root / f"raw-orientation-l{level}-b{band}.tif"
            gff.write_u16(raw_path, result.raw_winners[level][band][..., None])
            level_raw.append({"path": str(raw_path), "sha256": sha256_path(raw_path)})
            live = result.support[level][band]
            support_path = root / f"support-l{level}-b{band}.npy"
            np.save(support_path, live)
            level_support.append({"path": str(support_path), "sha256": sha256_path(support_path), "pixels": int(live.sum())})
        orientation.append(level_paths)
        raw.append(level_raw)
        support.append(level_support)
    pixel = None
    if result.pixel_sources is not None:
        path = root / "pixel-sources.tif"
        gff.write_u16(path, result.pixel_sources[..., None])
        pixel = {"path": str(path), "sha256": sha256_path(path)}
    return {"orientation_maps": orientation, "raw_orientation_maps": raw, "support_maps": support, "pixel_source_map": pixel}


def _wavelet_aligned_identity(row: dict[str, object]) -> dict[str, object] | None:
    """Bind retained aligned source and coverage files separately from frozen input identity."""
    paths = row.get("actual_source_frames")
    coverage = row.get("actual_coverage")
    if not paths or not coverage:
        return None
    mask = Path(str(coverage))
    source_paths = [Path(str(path)) for path in paths]
    if not mask.is_file() or any(not path.is_file() for path in source_paths):
        raise ValueError("retained aligned source or coverage file is missing")
    return {"paths": [str(path) for path in source_paths], "path_sha256": [sha256_path(path) for path in source_paths],
            "coverage_path": str(mask), "coverage_sha256": sha256_path(mask), "reference": row.get("alignment", {}).get("reference"),
            "transforms": row.get("alignment", {}).get("alignments")}


def _wavelet_selection_source(row: dict[str, object]) -> object:
    """Open retained frames and case coverage used for baseline scoring."""
    if row.get("actual_source_frames"):
        coverage = row.get("actual_coverage")
        if not isinstance(coverage, str) or not Path(coverage).is_file():
            raise ValueError("complex-wavelet aligned input lacks retained coverage mask")
        source = gff.TiffSource(row["actual_source_frames"], coverage)
    else:
        case = Path(str(row.get("case", "")))
        mask = case / "masks.npy"
        if row.get("family") == "invalid-borders" and row.get("member") == "zero-strip" and not mask.is_file():
            raise ValueError("complex-wavelet zero-strip input lacks required retained masks.npy")
        source = _source(case)
    if source.has_masks and (source.masks.dtype != bool or source.masks.shape != (source.n, source.h, source.w)):
        raise ValueError("complex-wavelet retained coverage has invalid type or dimensions")
    return source


def _wavelet_input_identity(row: dict[str, object]) -> dict[str, object]:
    """Bind the exact applicable coverage file independently of frame hashes."""
    source = _wavelet_selection_source(row)
    if not source.has_masks:
        return {"coverage": None}
    path = Path(str(row["actual_coverage"])) if row.get("actual_source_frames") else Path(str(row["case"])) / "masks.npy"
    return {"coverage": {"path": str(path), "sha256": sha256_path(path), "dtype": str(source.masks.dtype), "shape": list(source.masks.shape)}}


def complex_wavelet_case_controls(output: Path) -> list[dict[str, object]]:
    """Exercise physical case coverage and missing/malformed mask refusals."""
    case = output / "case-coverage"
    (case / "frames").mkdir(parents=True)
    for index in range(2):
        gff.write_u16(case / "frames" / f"{index:02}.tif", np.full((64, 96, 3), 6000 + index * 1000, dtype=np.uint16))
    masks = np.ones((2, 64, 96), dtype=bool)
    masks[0, :, :5] = False
    masks[1, :, :7] = False
    mask_path = case / "masks.npy"
    np.save(mask_path, masks)
    row = {"case": str(case), "family": "invalid-borders", "member": "zero-strip"}
    source = _wavelet_selection_source(row)
    assert source.has_masks and all(np.array_equal(source.read(index)[1], masks[index]) for index in range(2)), "retained case coverage omitted"
    bound = _wavelet_input_identity(row)["coverage"]
    assert bound["sha256"] == sha256_path(mask_path) and bound["shape"] == [2, 64, 96] and bound["dtype"] == "bool", "retained coverage identity differs"
    contamination = gff.border_pull(np.full((64, 96, 3), 5000, dtype=np.uint16), source)
    assert contamination == 1000.0, "mixed-validity border metric differs"
    refusals = []
    for name, value in (("mask-width", np.ones((2, 64, 95), dtype=bool)), ("mask-type", masks.astype(np.uint8)), ("mask-missing", None)):
        if value is None:
            mask_path.unlink()
        else:
            np.save(mask_path, value)
        try:
            _wavelet_selection_source(row)
        except ValueError as error:
            assert "coverage" in str(error) or "masks.npy" in str(error), str(error)
            refusals.append(name)
        else:
            raise AssertionError(f"{name}: invalid retained mask accepted")
    np.save(mask_path, masks)
    return [{"name": "physical-case-coverage", "shape_hwc": [64, 96, 3], "coverage_sha256": bound["sha256"], "mixed_pixels": 128, "border_contamination": contamination, "refusals": refusals}]


def _wavelet_label_maps(root: Path, result: complex_wavelet.FusionResult, shape: tuple[int, int]) -> list[dict[str, object]]:
    """Expand each finest coefficient plane at its global two-pixel anchors."""
    labels = []
    height, width = shape
    for band, winners in enumerate(result.orientation_winners[0]):
        expanded = np.repeat(np.repeat(winners, 2, axis=0), 2, axis=1)[:height, :width]
        support = np.repeat(np.repeat(result.support[0][band], 2, axis=0), 2, axis=1)[:height, :width]
        path, support_path = root / f"finest-orientation-{band}-labels.tif", root / f"finest-orientation-{band}-support.npy"
        gff.write_u16(path, expanded[..., None])
        np.save(support_path, support)
        labels.append({"orientation": band, "path": str(path), "sha256": sha256_path(path), "support_path": str(support_path),
                       "support_sha256": sha256_path(support_path), "valid_support_pixels": int(support.sum())})
    return labels


def _wavelet_record_metrics(data: SceneData, row: dict[str, object], composite: Path, labels: list[dict[str, object]],
                            source: object, tiled_difference: int) -> tuple[dict[str, float | None], dict[str, object], list[dict[str, object]]]:
    """Reuse frozen image metrics; score each orientation label plane independently."""
    margin = ((complex_wavelet.theoretical_reach() + 31) // 32) * 32
    boundaries = tile_boundaries(data.truth.shape[1], data.truth.shape[0], int(row["tile"]), margin, complex_wavelet.LEVELS)
    metrics_row = {**row, "tile_difference": {"max_abs": tiled_difference}}
    values, metadata = _record_metrics(data, str(row["family"]), str(row["member"]), metrics_row, composite, Path(str(labels[0]["path"])), source, boundaries)
    unavailable = dict(metadata["metric_unavailable"])
    for name in ("label_exact", "label_within_one", "label_mad"):
        values[name] = None
        unavailable[name] = "three orientation source-label metrics are recorded separately"
    for name in ("geometry_error", "alignment_drift", "alignment_failures"):
        values[name] = None
        unavailable[name] = "complex-wavelet synthetic comparison does not estimate alignment"
    label_truth, label_depth, label_support = data.truth, data.depth, np.ones(data.depth.shape, dtype=bool)
    if row["family"] == "invalid-borders" and row["member"] == "aligned-coverage":
        label_truth, label_depth, label_support = _rebase_truth(data, int(row["alignment"]["reference"]))
    truth = np.rint(label_depth).clip(0, 65535).astype(np.uint16)
    for name in ("gradient_energy", "map_coherence"):
        if values.get(name) is None:
            unavailable.setdefault(name, "complex-wavelet does not define this frozen diagnostic")
    for name, value in values.items():
        if value is None:
            unavailable.setdefault(name, "registered metric is nonfinite or its applicable truth region is empty")
    orientation_scores = []
    for item in labels:
        label = gff.read_zmap(Path(str(item["path"])))
        valid = label_support & np.load(Path(str(item["support_path"]))) & (label != fusion.NO_SOURCE)
        label_values = metrics.label_metrics(label, truth, valid)
        orientation_scores.append({"orientation": item["orientation"], "valid_support_pixels": int(valid.sum()),
                                   "metrics": label_values,
                                   "metric_unavailable": {name: "orientation has no eligible rebased truth support" for name, value in label_values.items() if value is None}})
    return values, {**metadata, "metric_unavailable": unavailable, "registered_metric_count": len(values)}, orientation_scores


def _wavelet_maps_equal(whole: complex_wavelet.FusionResult, tiled: complex_wavelet.FusionResult) -> dict[str, object]:
    final, raw, support = [], [], []
    for level in range(complex_wavelet.LEVELS):
        for band in range(3):
            final.append(int(np.count_nonzero(whole.orientation_winners[level][band] != tiled.orientation_winners[level][band])))
            raw.append(int(np.count_nonzero(whole.raw_winners[level][band] != tiled.raw_winners[level][band])))
            support.append(int(np.count_nonzero(whole.support[level][band] != tiled.support[level][band])))
    image = int(np.count_nonzero(whole.composite != tiled.composite))
    if (whole.pixel_sources is None) != (tiled.pixel_sources is None):
        raise AssertionError("complex-wavelet whole/tile reassignment presence mismatch")
    pixels = 0 if whole.pixel_sources is None else int(np.count_nonzero(whole.pixel_sources != tiled.pixel_sources))
    if image or pixels or any(final) or any(raw) or any(support):
        raise AssertionError(f"complex-wavelet whole/tile mismatch image={image} final={final} raw={raw} support={support}")
    return {"different_samples": image, "pixel_source_differences": pixels, "orientation_map_differences": final, "raw_map_differences": raw, "support_differences": support}


def run_complex_wavelet_configurations(root: Path, output: Path, configurations: list[dict[str, object]],
                                       family_filter: str | None = None, repeat: int = 0) -> list[dict[str, object]]:
    """Run exact four-mode whole/tile records over frozen repeat-zero Details inputs."""
    rows = [row for row in json.loads((root / "baseline.json").read_text())
            if row["repeat"] == 0 and row["method"] == "laplacian" and (family_filter is None or row["family"] == family_filter)]
    expected = {item["id"] for item in complex_wavelet.CONFIGURATIONS}
    if {str(item.get("id")) for item in configurations} != expected or len(configurations) != 4:
        raise ValueError("complex-wavelet requires exactly four registered configurations")
    records = []
    for configuration in configurations:
        for row in rows:
            source = _wavelet_selection_source(row)
            whole = complex_wavelet.fuse(source, configuration)
            tiled, plan = complex_wavelet.fuse_tiled(source, configuration, tile=int(row["tile"]))
            equality = _wavelet_maps_equal(whole, tiled)
            seed = SeedTriple.parse(str(row["seed"]))
            directory = output / str(configuration["id"]) / run_key(f"{row['family']}-{row['member']}", seed, "complex-wavelet", int(row["tile"]), repeat)
            directory.mkdir(parents=True, exist_ok=True)
            whole_path, tiled_path = directory / "whole.tif", directory / "tiled.tif"
            gff.write_u16(whole_path, whole.composite)
            gff.write_u16(tiled_path, tiled.composite)
            maps, tiled_maps = _wavelet_maps(directory / "whole-maps", whole), _wavelet_maps(directory / "tiled-maps", tiled)
            labels = _wavelet_label_maps(directory, whole, (source.h, source.w))
            family = next(item for item in FAMILIES if item.name == row["family"])
            data = render_family(family, seed, str(row["member"]))
            values, metric_metadata, orientation_scores = _wavelet_record_metrics(data, row, whole_path, labels, source, equality["different_samples"])
            pixel_scores, pixel_metadata = None, None
            if whole.pixel_sources is not None:
                label_depth, label_support = data.depth, np.ones(data.depth.shape, dtype=bool)
                if row["family"] == "invalid-borders" and row["member"] == "aligned-coverage":
                    _truth, label_depth, label_support = _rebase_truth(data, int(row["alignment"]["reference"]))
                valid = label_support & (whole.pixel_sources != fusion.NO_SOURCE)
                pixel_scores = metrics.label_metrics(whole.pixel_sources, np.rint(label_depth).clip(0, 65535).astype(np.uint16), valid)
                pixel_metadata = {"valid_support_pixels": int(valid.sum()), "source": "nearest-grey reassignment pixel map", "metric_unavailable": {name: "pixel reassignment has no eligible rebased truth support" for name, value in pixel_scores.items() if value is None}}
            records.append({"configuration_id": configuration["id"], "family": row["family"], "member": row["member"], "seed": row["seed"],
                            "tile": row["tile"], "repeat": repeat, "whole": str(whole_path), "whole_sha256": sha256_path(whole_path),
                            "tiled": str(tiled_path), "tiled_sha256": sha256_path(tiled_path), "native_whole_tile": equality,
                            "input_identity": row.get("input_sha256"), "coverage_identity": _wavelet_input_identity(row), "aligned_identity": _wavelet_aligned_identity(row), "vote_counts": whole.counts, "tile_plan": plan,
                            "coefficient_source_maps": maps["orientation_maps"], "tiled_coefficient_source_maps": tiled_maps["orientation_maps"],
                            "raw_coefficient_source_maps": maps["raw_orientation_maps"], "tiled_raw_coefficient_source_maps": tiled_maps["raw_orientation_maps"],
                            "support_maps": maps["support_maps"], "tiled_support_maps": tiled_maps["support_maps"], "pixel_source_map": maps["pixel_source_map"], "tiled_pixel_source_map": tiled_maps["pixel_source_map"],
                            "orientation_label_maps": labels, "orientation_label_metrics": orientation_scores,
                            "pixel_label_metrics": pixel_scores, "pixel_label_metadata": pixel_metadata,
                            "image_metrics": values, "metric_metadata": metric_metadata, "metric_count": len(values)})
    if family_filter is None and len(records) != 240:
        raise AssertionError("complex-wavelet training matrix must contain 240 rows")
    return records

def complex_wavelet_paired_effects(root: Path, records: list[dict[str, object]]) -> list[dict[str, object]]:
    """Compute signed candidate-minus-retained-baseline metrics without another fusion run."""
    baseline = json.loads((root / "baseline.json").read_text())
    indexed = {(row["family"], row["member"], row["seed"], row["tile"], row["method"]): row for row in baseline if row.get("repeat") == 0}
    effects = []
    for row in records:
        key = (row["family"], row["member"], row["seed"], row["tile"])
        for method in METHODS:
            reference = indexed.get((*key, method))
            if reference is None:
                raise ValueError(f"complex-wavelet missing retained {method} baseline for {key}")
            values = reference.get("metrics", {})
            signed = {name: (None if row["image_metrics"].get(name) is None or values.get(name) is None else float(row["image_metrics"][name]) - float(values[name]))
                      for name in row["image_metrics"]}
            labels = [{"orientation": item["orientation"], "signed": {name: (None if item["metrics"].get(name) is None or values.get(name) is None else float(item["metrics"][name]) - float(values[name])) for name in item["metrics"]}}
                      for item in row["orientation_label_metrics"]]
            pixel = None if row.get("pixel_label_metrics") is None else {name: (None if row["pixel_label_metrics"].get(name) is None or values.get(name) is None else float(row["pixel_label_metrics"][name]) - float(values[name])) for name in row["pixel_label_metrics"]}
            effects.append({"configuration_id": row["configuration_id"], "family": row["family"], "member": row["member"], "seed": row["seed"],
                            "tile": row["tile"], "baseline_method": method, "signed_effects": signed,
                            "orientation_label_effects": labels, "pixel_label_effect": pixel,
                            "pixel_label_unavailable": None if pixel is not None else "shared configuration has no pixel reassignment map"})
    indexed_candidates = {(row["configuration_id"], row["family"], row["member"], row["seed"], row["tile"]): row for row in records}
    for left_id, right_id, factor in (("shared-raw", "shared-voted", "consistency"), ("grey-reassign-raw", "grey-reassign-voted", "consistency"),
                                      ("shared-raw", "grey-reassign-raw", "colour-reassignment"), ("shared-voted", "grey-reassign-voted", "colour-reassignment")):
        for key in {(row["family"], row["member"], row["seed"], row["tile"]) for row in records}:
            left, right = indexed_candidates[(left_id, *key)], indexed_candidates[(right_id, *key)]
            signed = {name: (None if left["image_metrics"].get(name) is None or right["image_metrics"].get(name) is None else float(right["image_metrics"][name]) - float(left["image_metrics"][name])) for name in left["image_metrics"]}
            labels = [{"orientation": index, "signed": {name: (None if a["metrics"].get(name) is None or b["metrics"].get(name) is None else float(b["metrics"][name]) - float(a["metrics"][name])) for name in a["metrics"]}}
                      for index, (a, b) in enumerate(zip(left["orientation_label_metrics"], right["orientation_label_metrics"]))]
            pixel = None
            pixel_reason = "shared configuration has no pixel reassignment map"
            if left.get("pixel_label_metrics") is not None and right.get("pixel_label_metrics") is not None:
                pixel = {name: (None if left["pixel_label_metrics"].get(name) is None or right["pixel_label_metrics"].get(name) is None else float(right["pixel_label_metrics"][name]) - float(left["pixel_label_metrics"][name])) for name in left["pixel_label_metrics"]}
                pixel_reason = None
            effects.append({"factor": factor, "left_configuration": left_id, "right_configuration": right_id, "family": key[0], "member": key[1], "seed": key[2], "tile": key[3],
                            "signed_effects": signed, "orientation_label_effects": labels, "pixel_label_effect": pixel, "pixel_label_unavailable": pixel_reason})
    groups: dict[tuple[object, ...], list[dict[str, object]]] = defaultdict(list)
    for row in effects:
        comparison = ('baseline', row['configuration_id'], row['baseline_method']) if 'baseline_method' in row else ('factor', row['left_configuration'], row['right_configuration'])
        groups[(*comparison, row['family'], row['member'], row['tile'])].append(row)

    def seed_statistics(planes):
        names = next((plane for plane in planes if plane is not None), {})
        samples = {name: [float(plane[name]) for plane in planes if plane is not None and plane.get(name) is not None and math.isfinite(float(plane[name]))] for name in names}
        return {name: None if not values else float(np.std(values, ddof=0)) for name, values in samples.items()}, {name: len(values) for name, values in samples.items()}

    for group in groups.values():
        image_spread, image_count = seed_statistics([row['signed_effects'] for row in group])
        labels = [seed_statistics([row['orientation_label_effects'][orientation]['signed'] for row in group]) for orientation in range(3)]
        pixel_spread, pixel_count = seed_statistics([row['pixel_label_effect'] for row in group])
        for row in group:
            row['paired_seed_spread'] = image_spread
            row['orientation_paired_seed_spread'] = [item[0] for item in labels]
            row['pixel_paired_seed_spread'] = pixel_spread if row['pixel_label_effect'] is not None else None
            row['uncertainty'] = {'kind': 'population standard deviation across same-family keyed training seeds',
                                  'metric_spread': image_spread, 'orientation_spread': row['orientation_paired_seed_spread'], 'pixel_spread': row['pixel_paired_seed_spread'],
                                  'valid_seed_counts': {'image': image_count, 'orientation': [item[1] for item in labels], 'pixel': pixel_count if row['pixel_label_effect'] is not None else None}}
    return effects


def _wavelet_artifact_content(value: object) -> object:
    """Keep native content bindings while intentionally discarding output locations."""
    if isinstance(value, dict):
        return {key: _wavelet_artifact_content(item) for key, item in value.items() if key not in {"path", "support_path"}}
    if isinstance(value, list):
        return [_wavelet_artifact_content(item) for item in value]
    return value


def _wavelet_verify_file(path_value: object, digest: object, verified: set[tuple[str, str]]) -> None:
    """Verify each retained native file once before repeat output."""
    if not isinstance(path_value, str) or not isinstance(digest, str) or len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
        raise ValueError("complex-wavelet native file lacks a path and SHA256")
    path = Path(path_value)
    binding = (str(path.resolve()), digest)
    if binding not in verified:
        if not path.is_file() or sha256_path(path) != digest:
            raise ValueError(f"complex-wavelet native file is missing or stale: {path}")
        verified.add(binding)


def complex_wavelet_repeat_preflight(prior: list[dict[str, object]], baseline: list[dict[str, object]] | None = None) -> None:
    """Validate registered keys and every retained native artifact before a repeat."""
    members = (("thin-hairs", "default"), ("flat-noise", "noise-250"), ("flat-noise", "noise-1000"),
               ("colour-edges", "default"), ("invalid-borders", "zero-strip"), ("invalid-borders", "aligned-coverage"))
    expected = {(config["id"], family, member, seed.text(), tile) for config in complex_wavelet.CONFIGURATIONS
                for family, member in members for seed in TRAINING_SEEDS for tile in (512, 1024)}
    if not isinstance(prior, list) or any(not isinstance(row, dict) for row in prior):
        raise ValueError("complex-wavelet training rows must be a list of objects")
    if any(any(not isinstance(row.get(name), str) for name in ("configuration_id", "family", "member", "seed")) or not isinstance(row.get("tile"), int) or isinstance(row.get("tile"), bool) for row in prior):
        raise ValueError("complex-wavelet training keys have invalid scalar types")
    keys = {(row.get("configuration_id"), row.get("family"), row.get("member"), row.get("seed"), row.get("tile")) for row in prior}
    if len(prior) != 240 or keys != expected:
        raise ValueError("complex-wavelet repeat requires the exact unique 240 training keys")
    references = {(row["family"], row["member"], row["seed"], row["tile"]): row for row in baseline or []
                  if row.get("method") == "laplacian" and row.get("repeat") == 0}
    verified: set[tuple[str, str]] = set()
    for row in prior:
        if row.get("repeat") != 0 or not isinstance(row.get("input_identity"), dict) or not row["input_identity"]:
            raise ValueError("complex-wavelet repeat requires repeat-zero input identity")
        if not isinstance(row.get("coverage_identity"), dict) or "coverage" not in row["coverage_identity"]:
            raise ValueError("complex-wavelet repeat requires retained coverage identity")
        if baseline is not None:
            reference = references.get(tuple(row[name] for name in ("family", "member", "seed", "tile")))
            if (reference is None or row["input_identity"] != reference.get("input_sha256")
                    or row.get("coverage_identity") != _wavelet_input_identity(reference)
                    or row.get("aligned_identity") != _wavelet_aligned_identity(reference)):
                raise ValueError("complex-wavelet training input or aligned coverage identity is stale")
        for name in ("whole", "tiled"):
            _wavelet_verify_file(row.get(name), row.get(name + "_sha256"), verified)
        for name in ("coefficient_source_maps", "tiled_coefficient_source_maps", "raw_coefficient_source_maps", "tiled_raw_coefficient_source_maps", "support_maps", "tiled_support_maps"):
            planes = row.get(name)
            if not isinstance(planes, list) or len(planes) != 5 or any(not isinstance(bands, list) or len(bands) != 3 for bands in planes):
                raise ValueError(f"complex-wavelet repeat requires all five levels and three bands: {name}")
            for bands in planes:
                for plane in bands:
                    if not isinstance(plane, dict):
                        raise ValueError(f"complex-wavelet repeat native plane is invalid: {name}")
                    _wavelet_verify_file(plane.get("path"), plane.get("sha256"), verified)
        reassign = str(row["configuration_id"]).startswith("grey-reassign-")
        for name in ("pixel_source_map", "tiled_pixel_source_map"):
            plane = row.get(name)
            if reassign:
                if not isinstance(plane, dict):
                    raise ValueError("complex-wavelet repeat requires the reassignment pixel plane")
                _wavelet_verify_file(plane.get("path"), plane.get("sha256"), verified)
            elif name not in row or plane is not None:
                raise ValueError("complex-wavelet shared mode must explicitly have no reassignment plane")
        labels = row.get("orientation_label_maps")
        if not isinstance(labels, list) or len(labels) != 3 or {v.get("orientation") for v in labels if isinstance(v, dict)} != {0, 1, 2}:
            raise ValueError("complex-wavelet repeat requires three finest orientation label/support planes")
        for plane in labels:
            _wavelet_verify_file(plane.get("path"), plane.get("sha256"), verified)
            _wavelet_verify_file(plane.get("support_path"), plane.get("support_sha256"), verified)
        native = row.get("native_whole_tile")
        if not isinstance(native, dict) or native.get("different_samples") != 0 or native.get("pixel_source_differences") != 0 or any(native.get(name) != [0] * 15 for name in ("orientation_map_differences", "raw_map_differences", "support_differences")):
            raise ValueError("complex-wavelet training native whole/tile proof is incomplete")


def complex_wavelet_repeat_equality(records: list[dict[str, object]], prior: list[dict[str, object]]) -> list[dict[str, object]]:
    """Require native content and input identity match without comparing output paths."""
    complex_wavelet_repeat_preflight(prior)
    def key(row: dict[str, object]) -> tuple[object, ...]:
        return tuple(row[name] for name in ("configuration_id", "family", "member", "seed", "tile"))
    previous = {key(row): row for row in prior}
    if len(records) != 240 or set(previous) != {key(row) for row in records}:
        raise ValueError("complex-wavelet repeat lacks the exact 240-key training record")
    comparisons = []
    for row in records:
        first = previous[key(row)]
        fields = ("input_identity", "coverage_identity", "aligned_identity", "whole_sha256", "tiled_sha256", "coefficient_source_maps", "tiled_coefficient_source_maps", "raw_coefficient_source_maps", "tiled_raw_coefficient_source_maps", "support_maps", "tiled_support_maps", "pixel_source_map", "tiled_pixel_source_map", "orientation_label_maps")
        equal = all(_wavelet_artifact_content(row.get(field)) == _wavelet_artifact_content(first.get(field)) for field in fields)
        if not equal:
            raise AssertionError(f"complex-wavelet repeat native equality failed for {key(row)}")
        comparisons.append({"key": list(key(row)), "native_equal": True})
    return comparisons

def _wavelet_state_bound(source: object) -> dict[str, object]:
    """Quantify Python-array phase bounds; observed RSS remains a separate body receipt."""
    padded_h, padded_w = ((source.h + 31) // 32) * 32, ((source.w + 31) // 32) * 32
    details = [(padded_h // (2 ** (level + 1)), padded_w // (2 ** (level + 1)), source.c) for level in range(complex_wavelet.LEVELS)]
    coeff = sum(3 * int(np.prod(shape)) * np.dtype(np.complex128).itemsize for shape in details)
    scalar = sum(3 * shape[0] * shape[1] for shape in details)
    low_shape = (padded_h // 32, padded_w // 32, source.c)
    low_coeff = int(np.prod(low_shape)) * np.dtype(np.complex128).itemsize
    input_bytes = source.h * source.w * source.c * np.dtype(np.float64).itemsize
    padded_bytes = padded_h * padded_w * source.c * np.dtype(np.complex128).itemsize
    phases = {"analysis_current_and_six_tap_samples": {"current_padded_complex128": padded_bytes, "advanced_index_six_tap_transient_upper_bound": 6 * padded_bytes},
              "selection_persistent": {"best_float64": scalar * np.dtype(np.float64).itemsize, "raw_and_final_u16": 2 * scalar * np.dtype(np.uint16).itemsize,
                                       "seen_and_support_bool": 2 * scalar * np.dtype(bool).itemsize, "low_sum_complex128": low_coeff,
                                       "low_count_u16": low_shape[0] * low_shape[1] * np.dtype(np.uint16).itemsize},
              "gather": {"current_tree_complex128": coeff + low_coeff, "selected_tree_complex128": coeff + low_coeff, "transient_gathered_tree_complex128": coeff + low_coeff},
              "synthesis": {"selected_tree_complex128": coeff + low_coeff, "synthesis_padded_complex128": padded_bytes},
              "reassign": {"input_rgb_float64": input_bytes, "distance_float64": source.h * source.w * np.dtype(np.float64).itemsize,
                           "pixel_source_u16": source.h * source.w * np.dtype(np.uint16).itemsize, "output_rgb_float64": input_bytes}}
    # Prior/current trees and reader-normalization buffers can overlap across loop phases.
    resident = {"resident_input_float64": input_bytes, "reader_raw_u16": input_bytes // 4,
                "reader_normalized_float32": input_bytes // 2, "reflected_samples_float64": padded_bytes // 2,
                "input_validation_bool": source.h * source.w * source.c,
                "padded_eligibility_bool": padded_h * padded_w,
                "persistent_selection_state": sum(phases["selection_persistent"].values()),
                "prior_frame_tree_held": coeff + low_coeff, "current_frame_tree_upper_bound": coeff + low_coeff,
                "resident_output_u16": input_bytes // 4, "resident_gray_float64": source.h * source.w * 8}
    for values in phases.values():
        values.update(resident)
    totals = {name: int(sum(values.values())) for name, values in phases.items()}
    return {"kind": "structural_python_ndarray_upper_bound_not_observed_rss", "shape_hwc": [source.h, source.w, source.c], "padded_shape_hwc": [padded_h, padded_w, source.c],
            "phase_bytes": phases, "phase_total_bytes": totals, "maximum_phase_bytes": max(totals.values()),
            "n_dependent_metadata": {"declared_path_references": "O(N)", "tiff_infos": "O(N)", "reader_pass_counters": "O(N)"},
            "metadata_object_counts": {"paths": len(getattr(source, "paths", ())), "tiff_infos": len(getattr(source, "infos", ())), "read_counters": len(getattr(source, "reads", ()))},
            "bound_overcount": "reader buffers and prior/current trees are conservatively included in every phase; bounds are not observed allocations",
            "cache_limitation": "OS page cache and NumPy C temporaries are not controlled or claimed as RSS", "prohibited": ["N-sized pixel cache", "N-sized coefficient cache"]}


def run_complex_wavelet_stack(output: Path, source_name: str, prepared: Path | None, rect_text: str | None,
                              configurations: list[dict[str, object]]) -> dict[str, object]:
    """Fuse only the registered aligned real crop after validating its preparation."""
    if prepared is None:
        raise ValueError("--prepared preparation-manifest.json is required before complex-wavelet real output")
    if {item.get("id") for item in configurations} != {item["id"] for item in complex_wavelet.CONFIGURATIONS} or len(configurations) != 4:
        raise ValueError("complex-wavelet real mode requires exactly four registered configurations")
    rect = _registered_rect(source_name, rect_text)
    manifest = _validate_preparation(prepared, source_name)
    prepared_sha = sha256_path(prepared)
    retained_path = prepared.parents[3] / "stacking-crops" / source_name / "crops.json"
    if not retained_path.is_file():
        raise ValueError("retained Details crop reference is required before complex-wavelet output")
    retained = json.loads(retained_path.read_text())
    details = next((row for row in retained if isinstance(row, dict) and row.get("method") == "laplacian"
                    and row.get("prepared_manifest_sha256") == prepared_sha and row.get("alignment_report_sha256") == manifest["alignment_report_sha256"]
                    and row.get("rect_xywh") == list(rect)), None)
    if (details is None or details.get("raw_inputs") != manifest["raw_inputs"] or details.get("aligned_inputs") != manifest["aligned_inputs"]
            or details.get("coverage", {}).get("all_sources_cover_roi") is not True):
        raise ValueError("retained Details crop reference does not match preparation geometry")
    details_composite = Path(str(details.get("composite", "")))
    if not details_composite.is_file() or details.get("composite_sha256") != sha256_path(details_composite):
        raise ValueError("retained Details crop composite is missing or stale")
    directory, paths, coverage = _wavelet_crop_prepared_inputs(output, source_name, prepared, rect)
    source = gff.TiffSource(paths, Path(str(coverage["mask_path"])))
    null_fields = ("psnr", "ssim", "edge_halo", "hair_retention", "flat_noise", "chroma_drift", "border_contamination", "seam_energy", "gradient_energy", "map_coherence", "label_exact", "label_within_one", "label_mad", "tile_difference", "geometry_error", "alignment_drift", "alignment_failures")
    null_reasons = {name: "registered real crop has no synthetic truth or source-index truth" for name in null_fields}
    rows = []
    for configuration in configurations:
        result = complex_wavelet.fuse(source, configuration)
        work = output / "real" / str(configuration["id"])
        work.mkdir(parents=True, exist_ok=True)
        composite = work / "composite.tif"
        gff.write_u16(composite, result.composite)
        montage = work / "montage.png"
        gff.montage(str(montage), (0, source.h, 0, source.w), [details_composite, composite])
        rendered = Image.open(montage).convert("RGB")
        canvas = Image.new("RGB", (rendered.width, rendered.height + 24), "black")
        canvas.paste(rendered, (0, 24))
        ImageDraw.Draw(canvas).text((4, 4), f"retained Details | {configuration['id']} | crop-only, no truth", fill="white")
        canvas.save(montage)
        rows.append({"configuration_id": configuration["id"], "composite": str(composite), "composite_sha256": sha256_path(composite),
                     "montage": str(montage), "montage_sha256": sha256_path(montage), "montage_labels": ["retained Details", str(configuration["id"])],
                     "retained_details_composite": str(details_composite), "retained_details_composite_sha256": sha256_path(details_composite),
                     "maps": _wavelet_maps(work, result), "rect_xywh": list(rect), "coverage": coverage,
                     "prepared_manifest_sha256": sha256_path(prepared), "alignment_report_sha256": manifest["alignment_report_sha256"],
                     "raw_inputs": manifest["raw_inputs"], "aligned_inputs": manifest["aligned_inputs"],
                     "image_metrics": {name: None for name in null_fields}, "metric_unavailable": null_reasons,
                     "metric_count": len(null_fields), "truth_metrics": "not computed"})
    return {"source": source_name, "rect_xywh": list(rect), "prepared_manifest": str(prepared.resolve()), "rows": rows}

def run_complex_wavelet_resource_body(runs: Path, output: Path, frame_count: int) -> dict[str, object]:
    """Measure one fresh-process bounded-memory body from registered physical TIFFs."""
    if frame_count not in {8, 32}:
        raise ValueError("complex-wavelet resource frame count must be 8 or 32")
    rows = [row for row in json.loads((runs / "baseline.json").read_text())
            if row["repeat"] == 0 and row["method"] == "laplacian" and row["tile"] == 512 and row["family"] == "thin-hairs"]
    if not rows:
        raise ValueError("registered physical 512px resource source is missing")
    base = _selection_source(rows[0])
    paths = [Path(path) for path in getattr(base, "paths", ())]
    if not paths:
        raise ValueError("complex-wavelet resource source must be TIFF-backed")
    declared = (paths * ((frame_count + len(paths) - 1) // len(paths)))[:frame_count]
    source = gff.TiffSource(declared)
    result = complex_wavelet.fuse(source, complex_wavelet.CONFIGURATIONS[0])
    composite = output / "composite.tif"
    output.mkdir(parents=True, exist_ok=True)
    gff.write_u16(composite, result.composite)
    return {"frame_count": frame_count, "configuration_id": complex_wavelet.CONFIGURATIONS[0]["id"], "physical_input_pool_paths": [str(path) for path in paths], "physical_input_pool_count": len(paths), "physical_input_pool": [{"path": str(path), "sha256": sha256_path(path)} for path in paths], "input_shape_hwc": [source.h, source.w, source.c],
            "declared_references": [str(path) for path in declared], "declared_reference_count": len(declared),
            "read_counts": source.reads, "composite": str(composite), "composite_sha256": sha256_path(composite),
            "state_bound": _wavelet_state_bound(source), "o_n_state": "TIFF header metadata, source paths and read counters only", "vote_counts": result.counts}


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
