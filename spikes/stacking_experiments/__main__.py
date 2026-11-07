# SPDX-License-Identifier: GPL-3.0-or-later
"""Command interface for fixed stack-experiment controls."""

from __future__ import annotations

import argparse
import math
import hashlib
import json
import os
import re
import subprocess
from pathlib import Path
import sys
import time

import numpy as np
import PIL

from . import alignment_reference, coefficient_shrinkage, complex_wavelet, consistency, focus_score, fusion, index_field, mask_pyramid, noise_score, photometric
from .protocol import FAMILIES, HOLDOUT_SEEDS, TRAINING_SEEDS, SeedTriple, registration, require_seed, validate_registration
from .scenes import family_members, render_family
from .runner import gff, _wavelet_verify_file, _wavelet_input_identity, complex_wavelet_case_controls, _twin_disagreements, _validate_preparation, baseline_call_count, case_split, focus_configuration_checks, focus_support_records, focus_tile_checks, index_calibration, index_field_tile_checks, index_replay_controls, mask_pyramid_gff_context, mask_pyramid_references, mask_pyramid_resource_argv, mask_pyramid_resource_controls, noise_shrinkage_resource_controls, noise_shrinkage_weighted_references, photometric_full_frame_manifest, photometric_physical_controls, photometric_real_roi_coverage, photometric_repeat_equality, photometric_repeat_preflight, photometric_stack_command, preparation_self_check, prepare_case, prepare_real, replay_control_checks, replay_focus_baselines, complex_wavelet_paired_effects, complex_wavelet_repeat_equality, complex_wavelet_repeat_preflight, run_complex_wavelet_configurations, run_complex_wavelet_resource_body, run_complex_wavelet_stack, run_crops, run_detector_controls, run_focus_configurations, run_focus_stack, run_focused_twins, run_baselines, run_equalized_context, run_index_configurations, run_index_full_references, run_index_stack, run_mask_pyramid_configurations, run_mask_pyramid_stack, run_noise_shrinkage_configurations, run_noise_shrinkage_stack, run_photometric_source_controls, run_photometric_stage, run_selection_configurations, run_selection_replay, run_selection_stack, selection_streaming_resource_controls, selection_tile_self_checks, selection_width_allowances, selection_width_measurements


def _complex_wavelet_semantics() -> dict[str, object]:
    """Return fixed transform and comparison settings for this arm."""
    return {"arm": "complex-wavelet", "transform": "Lina-Mayrand symmetric six-tap complex Daubechies", "levels": 5,
            "orientations": 3, "representation": "critically sampled complex coefficients", "analysis_phase": "2k+j-3 periodic",
            "synthesis": "conjugate adjoint", "padding": "bottom-right reflect; padded samples ineligible",
            "score": "unwindowed summed channel magnitude squared", "grey": "arithmetic RGB mean", "lowpass": "eligible-source mean",
            "configurations": list(complex_wavelet.CONFIGURATIONS), "selection": {"ties": "first eligible strict-greater", "detail_tuple": "one RGB source tuple per band", "lowpass": "eligible-source mean", "vote": "two-of-three orientations with second streaming gather", "grey": "nearest eligible grey source colour reassignment"},
            "maps": {"orientation": "three global two-pixel-anchor winner/support maps", "pixel_reassignment": "separate from coefficient winners", "uncovered": "NO_SOURCE distinct from unsupported"},
            "tile": {"outer_domain": "global padded periodic", "phase_pixels": 32, "margin": "four-config empirical impulse proof before body", "insufficient_interior": "refuse"},
            "real": {"manifest_sha256": "c61d135c8ca1e25d7d53b6b3db1bc5d7c9d1cbad1bd9e5d660a65b10f88ec405", "report_sha256": "bc4ca98f0d6bb5e980ace9c4ae57fd5ee5ecc03c5c99623144fb947438aa9ce3", "raw_indices": [0, 8, 17, 26, 34, 43, 52, 60], "reference": 3, "rect_xywh": [1878, 1056, 1024, 1024], "common_covered_pixels": 1048576, "processing_origin": "aligned registered crop only", "truth": "unavailable", "montages": 4, "null_reasons": 17},
            "metrics": {"count": 17, "definitions": registration()["metrics"], "shared_protocol_sha256": "e6faafb9539343a1579ca71f9d2cdac00b8a7395f8c64a05ec720eaaa7f54ab6", "floors": "unchanged keyed Details allowances"},
            "resource_procedure": {"bodies": 6, "counts": [8, 32], "repeats": 3, "fresh_process": True, "source": "registered physical TIFF", "no_pixel_or_coefficient_n_cache": True},
            "limits": {"total_seconds": 21600, "sessions": [7200, 7200, 7200], "source_controls_seconds": 1800, "training_calls": 240, "repeat_calls": 240, "resource_seconds": 600, "training_seconds": 6000, "repeat_seconds": 6000, "real_seconds": 1800, "gates_seconds": 3000, "admin_reserve_seconds": 600, "post_reserve_seconds": 60, "prior_verified_runtime_seconds": 726.10, "unassigned_reserve_seconds": 1013.90},
            "runtime": {"python": "3.14.4", "numpy": "2.3.5", "pillow": "12.1.1"},
            "training": {"holdout": "sealed", "tiles": [512, 1024], "seeds": [item.text() for item in TRAINING_SEEDS]}}


def _complex_wavelet_identity(document: dict[str, object], modules: dict[str, str], dependencies: dict[str, str]) -> str:
    """Hash immutable source and scientific bindings across readiness files."""
    policy = {key: document.get(key) for key in ("hypothesis", "benefit_conjunction", "benefit_key_coverage", "benefit_rule", "hard_veto")}
    payload = {"semantics": _complex_wavelet_semantics(), "modules": modules, "dependencies": dependencies,
               "baseline_sha256": document.get("baseline_sha256"), "allowances_sha256": document.get("allowances_sha256"), "policy": policy, "baseline_keys": document.get("baseline_keys"),
               "shared_protocol": document.get("shared_protocol"), "metric_assessor": document.get("metric_assessor"), "resource_input_pool": document.get("resource_input_pool"),
               "training_comparison": document.get("training_comparison"), "training_coverage": document.get("training_coverage")}
    return hashlib.sha256(json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def _validate_complex_wavelet_resources(artifact: object, identity: str, source_sha: str, verified: set[tuple[str, str]], expected_pool: list[dict[str, str]] | None = None) -> None:
    """Require six native timed bodies over the same physical input pool."""
    if not isinstance(artifact, dict) or artifact.get("body_count") != 6 or artifact.get("execution_identity") != identity or artifact.get("registration_sha256") != source_sha:
        raise ValueError("complex-wavelet resource artifact lacks six bound native bodies")
    rows = artifact.get("records")
    if not isinstance(rows, list) or len(rows) != 6 or any(not isinstance(row, dict) for row in rows):
        raise ValueError("complex-wavelet resource records are invalid")
    if any(any(not isinstance(row.get(name), int) or isinstance(row.get(name), bool) for name in ("frame_count", "repeat")) for row in rows):
        raise ValueError("complex-wavelet resource keys have invalid scalar types")
    if {(row.get("frame_count"), row.get("repeat")) for row in rows} != {(n, r) for n in (8, 32) for r in range(3)}:
        raise ValueError("complex-wavelet resource keys are missing or duplicated")
    pids, pools, phase_totals = [], [], []
    for row in rows:
        if row.get("native_exit") != 0:
            raise ValueError("complex-wavelet resource body failed")
        for name, path_key, hash_key in (("argv", "argv_path", "argv_sha256"), ("body", "body_receipt", "body_receipt_sha256"),
                                      ("stdout", "stdout", "stdout_sha256"), ("stderr", "stderr", "stderr_sha256"),
                                      ("time", "time_v", "time_v_sha256"), ("exit", "exit_path", "exit_sha256")):
            _wavelet_verify_file(row.get(path_key), row.get(hash_key), verified)
        if Path(row["exit_path"]).read_text().strip() != "0":
            raise ValueError("complex-wavelet resource native exit receipt failed")
        timing = Path(row["time_v"]).read_text()
        rss = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", timing)
        if not re.search(r"Exit status:\s*0\s*(?:$|\n)", timing) or rss is None or int(rss[1]) <= 0:
            raise ValueError("complex-wavelet resource GNU time/RSS receipt is incomplete")
        argv = json.loads(Path(row["argv_path"]).read_text())
        if argv != row.get("argv") or not isinstance(argv, list) or "--resource-body" not in argv or argv.index("--resource-body") + 1 >= len(argv) or str(row["frame_count"]) != argv[argv.index("--resource-body") + 1]:
            raise ValueError("complex-wavelet resource command identity differs")
        receipt = json.loads(Path(row["body_receipt"]).read_text())
        if not isinstance(receipt, dict) or receipt.get("registration_sha256") != source_sha or receipt.get("execution_identity") != identity:
            raise ValueError("complex-wavelet resource body source identity differs")
        pid = receipt.get("pid")
        if not isinstance(pid, int) or isinstance(pid, bool) or pid <= 0:
            raise ValueError("complex-wavelet resource body has no native PID")
        pids.append(pid)
        body = receipt.get("body")
        if not isinstance(body, dict) or body.get("frame_count") != row["frame_count"] or body.get("input_shape_hwc") != [512, 512, 3]:
            raise ValueError("complex-wavelet resource physical geometry differs")
        pool = body.get("physical_input_pool")
        if not isinstance(pool, list) or len(pool) != 8 or any(not isinstance(bound, dict) for bound in pool):
            raise ValueError("complex-wavelet resource physical input pool is incomplete")
        if expected_pool is not None and pool != expected_pool:
            raise ValueError("complex-wavelet resource pool differs from registered baseline files")
        for bound in pool:
            _wavelet_verify_file(bound.get("path"), bound.get("sha256"), verified)
            header = gff.rawtiff.read_info(Path(bound["path"]))
            if (header.height, header.width, header.samples_per_pixel) != (512, 512, 3) or header.bits_per_sample != (16, 16, 16):
                raise ValueError("complex-wavelet resource input header is not physical 512px RGB16")
        pools.append(pool)
        references = body.get("declared_references")
        if references != [bound["path"] for bound in pool] * (row["frame_count"] // 8):
            raise ValueError("complex-wavelet resource declared reference order differs")
        reads = body.get("read_counts")
        if not isinstance(reads, list) or len(reads) != row["frame_count"] or any(not isinstance(value, int) or value < 2 for value in reads):
            raise ValueError("complex-wavelet resource streaming read proof is incomplete")
        _wavelet_verify_file(body.get("composite"), body.get("composite_sha256"), verified)
        state = body.get("state_bound", {})
        if not isinstance(state, dict):
            raise ValueError("complex-wavelet resource source lifetime state is invalid")
        phases = state.get("phase_total_bytes")
        if not isinstance(phases, dict) or not phases or any(not isinstance(value, int) or value <= 0 for value in phases.values()):
            raise ValueError("complex-wavelet resource source lifetime bound is missing")
        phase_totals.append(phases)
    if len(set(pids)) != 6 or any(pool != pools[0] for pool in pools) or any(phases != phase_totals[0] for phases in phase_totals):
        raise ValueError("complex-wavelet resource fresh processes, physical pool or bounded array state differ")


def _validate_complex_wavelet_command(registration_path: Path, runs: Path, allowances: Path, resource_only: bool) -> dict[str, object]:
    """Reject incomplete source or quality registrations before writing output."""
    if not registration_path.is_file() or not (runs / "baseline.json").is_file() or not allowances.is_file():
        raise ValueError("--registration, --runs baseline.json, and --allowances are required")
    try:
        document = json.loads(registration_path.read_text())
    except json.JSONDecodeError as error:
        raise ValueError("complex-wavelet registration is not valid JSON") from error
    if not isinstance(document, dict):
        raise ValueError("complex-wavelet registration must be a JSON object")
    if any(document.get(key) != value for key, value in _complex_wavelet_semantics().items()):
        raise ValueError("complex-wavelet registration does not match immutable semantics")
    runtime = _complex_wavelet_semantics()["runtime"]
    if {"python": sys.version.split()[0], "numpy": np.__version__, "pillow": PIL.__version__} != runtime:
        raise ValueError("complex-wavelet runtime does not match the registered executable environment")
    policy = {"hypothesis": "Magnitude-squared maximum selection over Lina-Mayrand complex coefficients improves registered thin-hair and halo quality against retained Details, subject to all unchanged keyed objective floors. Consistency and colour reassignment are paired factors.",
              "benefit_key_coverage": "every applicable registered training key; no aggregate mean substitution",
              "benefit_rule": "each claimed metric must exceed two unchanged keyed Details allowances on every applicable training key",
              "hard_veto": "any applicable objective loss greater than one unchanged keyed Details allowance on any training seed; each orientation label result checked separately"}
    if (any(document.get(key) != value for key, value in policy.items())
            or document.get("benefit_conjunction") != "AND"
            or not isinstance(document.get("readiness_proofs"), dict)):
        raise ValueError("complex-wavelet registration has no settled immutable benefit and hard-veto policy")
    verified: set[tuple[str, str]] = set()
    for name in ("shared_protocol", "metric_assessor"):
        bound = document.get(name)
        if not isinstance(bound, dict):
            raise ValueError(f"complex-wavelet registration lacks {name} file binding")
        _wavelet_verify_file(bound.get("path"), bound.get("sha256"), verified)
    shared = json.loads(Path(document["shared_protocol"]["path"]).read_text())
    if document["shared_protocol"]["sha256"] != document["metrics"]["shared_protocol_sha256"] or shared.get("metrics") != registration()["metrics"]:
        raise ValueError("complex-wavelet shared metric definitions are stale")
    package = Path(__file__).parent
    names = [f"spikes/stacking_experiments/{name}" for name in ("__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py", "focus_score.py", "index_field.py", "consistency.py", "coefficient_shrinkage.py", "mask_pyramid.py", "complex_wavelet.py")]
    current = {name: hashlib.sha256((package / Path(name).name).read_bytes()).hexdigest() for name in names}
    dependencies = {name: hashlib.sha256((package.parents[1] / name).read_bytes()).hexdigest() for name in
                    ("spikes/guided-filter-fusion/gff_fusion.py", "spikes/synthetic-ground-truth/synth_stack.py", "tools/lib/rawtiff.py", "tools/tiff-diff.py")}
    if document.get("canonical_modules") != names or document.get("execution_module_sha256") != current or document.get("dependency_sha256") != dependencies:
        raise ValueError("complex-wavelet registration source bindings are stale")
    if (document.get("baseline_sha256") != "9f9d1e1f3195b4841180648617305aea4e2be3c98a49a815b4f6184f0bfc084c"
            or document.get("allowances_sha256") != "eb2086eb35d94b7254016ecbe10628a6a52ea5cc9b2d9544b02b4f93c418a5dc"):
        raise ValueError("complex-wavelet registration changed immutable baseline or allowances")
    if hashlib.sha256((runs / "baseline.json").read_bytes()).hexdigest() != document.get("baseline_sha256"):
        raise ValueError("complex-wavelet baseline records do not match registration")
    focus_score._validate_baseline_inputs(runs / "baseline.json")
    baseline_rows = json.loads((runs / "baseline.json").read_text())
    keys = [{"family": row["family"], "member": row["member"], "seed": row["seed"], "tile": row["tile"]}
            for row in baseline_rows if row.get("method") == "laplacian" and row.get("repeat") == 0]
    keys.sort(key=lambda item: (item["family"], item["member"], item["seed"], item["tile"]))
    expected_keys = {(family.name, member, seed.text(), tile) for family in FAMILIES if family.fusion_baseline
                     for member in family_members(family) for seed in TRAINING_SEEDS for tile in (512, 1024)}
    if (len(keys) != 60 or {(item["family"], item["member"], item["seed"], item["tile"]) for item in keys} != expected_keys
            or {item["tile"] for item in keys} != {512, 1024}
            or {item["seed"] for item in keys} != {seed.text() for seed in TRAINING_SEEDS}
            or document.get("baseline_keys") != keys):
        raise ValueError("complex-wavelet registration does not bind the exact 60 retained Details keys")
    coverage = [{**{name: row[name] for name in ("family", "member", "seed", "tile")}, **_wavelet_input_identity(row)}
                for row in baseline_rows if row.get("method") == "laplacian" and row.get("repeat") == 0]
    coverage.sort(key=lambda row: tuple(row[name] for name in ("family", "member", "seed", "tile")))
    if document.get("training_coverage") != coverage:
        raise ValueError("complex-wavelet registration training coverage is missing or stale")
    resource_row = next(row for row in baseline_rows if row.get("method") == "laplacian" and row.get("repeat") == 0 and row.get("family") == "thin-hairs" and row.get("seed") == "101,201,301" and row.get("tile") == 512)
    expected_pool = [{"path": str(Path(resource_row["case"]) / "frames" / name), "sha256": digest} for name, digest in sorted(resource_row["input_sha256"].items())]
    if document.get("resource_input_pool") != expected_pool:
        raise ValueError("complex-wavelet resource pool differs from the registered retained inputs")
    if not isinstance(document.get("training_comparison"), str) or not document["training_comparison"]:
        raise ValueError("complex-wavelet registration needs the prospective training comparison locator")
    if hashlib.sha256(allowances.read_bytes()).hexdigest() != document.get("allowances_sha256"):
        raise ValueError("complex-wavelet allowances do not match registration")
    identity = _complex_wavelet_identity(document, current, dependencies)
    if document.get("execution_identity") != identity:
        raise ValueError("complex-wavelet registration execution identity is stale")
    proof = document["readiness_proofs"]
    expected_proof = {"receipt", "receipt_sha256", "source_identity", "native_exit", "argv_path", "argv_sha256", "stdout_path", "stdout_sha256", "stderr_path", "stderr_sha256", "time_v_path", "time_v_sha256"}
    proof_path = Path(str(proof.get("receipt", "")))
    if (set(proof) != expected_proof or not proof_path.is_file()
            or hashlib.sha256(proof_path.read_bytes()).hexdigest() != proof.get("receipt_sha256")
            or proof.get("source_identity") != identity or proof.get("native_exit") != 0):
        raise ValueError("complex-wavelet registration lacks a valid source correctness receipt")
    raw_paths = {"argv": Path(str(proof["argv_path"])), "stdout": Path(str(proof["stdout_path"])),
                 "stderr": Path(str(proof["stderr_path"])), "time_v": Path(str(proof["time_v_path"]))}
    for name, path in raw_paths.items():
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != proof[f"{name}_sha256"]:
            raise ValueError(f"complex-wavelet source receipt {name} file is missing or stale")
    try:
        proof_document = json.loads(proof_path.read_text())
    except json.JSONDecodeError as error:
        raise ValueError("complex-wavelet source correctness receipt is invalid JSON") from error
    if not isinstance(proof_document, dict):
        raise ValueError("complex-wavelet source correctness receipt must be a JSON object")
    if (proof_document.get("execution_identity") != identity or proof_document.get("native_exit") != 0
            or any(proof_document.get(f"{name}_sha256") != proof[f"{name}_sha256"] for name in raw_paths)
            or any(Path(str(proof_document.get(f"{name}_path", ""))).resolve() != path.resolve() for name, path in raw_paths.items())):
        raise ValueError("complex-wavelet source correctness receipt does not bind this execution")
    if not re.search(r"Exit status:\s*0\s*(?:$|\n)", raw_paths["time_v"].read_text()):
        raise ValueError("complex-wavelet source GNU receipt does not show successful native exit")
    if not raw_paths["argv"].read_text().strip() or not raw_paths["stdout"].read_text().strip():
        raise ValueError("complex-wavelet source command or positive control output is empty")
    if document.get("execution_ready") is not True:
        raise ValueError("complex-wavelet registration is not source-ready")
    if resource_only:
        if document.get("quality_ready") is not False:
            raise ValueError("complex-wavelet resource mode requires immutable SOURCE_READY registration")
    else:
        resources = document.get("resources")
        if document.get("quality_ready") is not True or not isinstance(resources, dict):
            raise ValueError("complex-wavelet quality mode requires immutable QUALITY_READY registration")
        source = Path(str(resources.get("source_registration", "")))
        if source.resolve() == registration_path.resolve():
            raise ValueError("complex-wavelet quality proof must name a separate immutable source registration")
        artifact = Path(str(resources.get("resource_artifact", "")))
        audit = Path(str(resources.get("audit_receipt", "")))
        if (not source.is_file() or not artifact.is_file() or not audit.is_file()
                or hashlib.sha256(source.read_bytes()).hexdigest() != resources.get("source_registration_sha256")
                or hashlib.sha256(artifact.read_bytes()).hexdigest() != resources.get("resource_artifact_sha256")
                or hashlib.sha256(audit.read_bytes()).hexdigest() != resources.get("audit_receipt_sha256")):
            raise ValueError("complex-wavelet resource proof or audit is missing or stale")
        try:
            source_document, audit_document = json.loads(source.read_text()), json.loads(audit.read_text())
            source_checked = _validate_complex_wavelet_command(source, runs, allowances, True)
        except (json.JSONDecodeError, ValueError) as error:
            raise ValueError("complex-wavelet immutable SOURCE_READY registration is invalid") from error
        if not isinstance(source_document, dict) or not isinstance(audit_document, dict):
            raise ValueError("complex-wavelet readiness source registration or audit is invalid")
        if (source_document.get("quality_ready") is not False or source_document.get("execution_ready") is not True
                or source_checked.get("_execution_identity") != identity
                or _complex_wavelet_identity(source_document, current, dependencies) != identity
                or audit_document.get("passed") is not True or resources.get("execution_identity") != identity
                or audit_document.get("execution_identity") != identity
                or audit_document.get("source_registration") != str(source.resolve())
                or audit_document.get("source_registration_sha256") != resources.get("source_registration_sha256")
                or audit_document.get("resource_artifact") != str(artifact.resolve())
                or audit_document.get("resource_artifact_sha256") != resources.get("resource_artifact_sha256")):
            raise ValueError("complex-wavelet readiness evidence does not bind this execution")
        artifact_document = json.loads(artifact.read_text())
        _validate_complex_wavelet_resources(artifact_document, identity, resources["source_registration_sha256"], verified, expected_pool)
        if audit_document.get("resource_body_count") != 6 or audit_document.get("source_lifetime_pass") is not True or audit_document.get("native_receipts_pass") is not True:
            raise ValueError("complex-wavelet resource audit has not verified six bodies and source lifetime")
    document["_validated_registration_sha256"] = hashlib.sha256(registration_path.read_bytes()).hexdigest()
    document["_execution_identity"] = identity
    return document


def _complex_wavelet_guard_checks(template_path: Path, runs: Path, allowances: Path, output: Path) -> list[dict[str, object]]:
    """Check constructed native schemas and refusals without measured wavelet bodies."""
    import copy
    import contextlib
    import io
    from . import runner
    cli = sys.modules[__name__]
    OUT = output / "guard-controls"
    OUT.mkdir(exist_ok=False)
    records_out = []
    sha=lambda p:hashlib.sha256(Path(p).read_bytes()).hexdigest()
    def write(path,value):
        path.write_text(json.dumps(value,indent=2)+'\n');return path

    def native(directory,payload):
        directory.mkdir()
        write(directory/'payload.json',payload)
        script=directory/'child.py'
        script.write_text("import json,os,sys\nfrom pathlib import Path\np=Path(__file__).parent\nd=json.loads((p/'payload.json').read_text());d['pid']=os.getpid();(p/'body.json').write_text(json.dumps(d));print('constructed guard fixture; no wavelet workload')\n")
        argv=['/usr/bin/time','-v','-o',str(directory/'time.txt'),sys.executable,str(script),'--resource-body',str(payload.get('body',{}).get('frame_count',8))]
        write(directory/'argv.json',argv)
        with (directory/'stdout.txt').open('w') as out,(directory/'stderr.txt').open('w') as err:
            result=subprocess.run(argv,stdout=out,stderr=err)
        (directory/'exit.txt').write_text(str(result.returncode)+'\n');assert result.returncode==0
        return argv

    baseline=json.loads((runs/'baseline.json').read_text())
    details=[r for r in baseline if r['method']=='laplacian' and r['repeat']==0]
    keys=[{k:r[k] for k in ('family','member','seed','tile')} for r in details]
    keys.sort(key=lambda r:tuple(r[k] for k in ('family','member','seed','tile')))
    doc=json.loads(template_path.read_text())
    if not isinstance(doc, dict):
        raise ValueError("complex-wavelet control template must be a JSON object")
    doc.update(test_fixture=True,execution_ready=True,quality_ready=False)
    identity=doc['execution_identity']
    control=OUT/'control';native(control,{'execution_identity':identity})
    proof={'source_identity':identity,'native_exit':0}
    receipt={'execution_identity':identity,'native_exit':0,'test_fixture':True}
    for name,file in [('argv','argv.json'),('stdout','stdout.txt'),('stderr','stderr.txt'),('time_v','time.txt')]:
        path=control/file;proof[name+'_path']=str(path);proof[name+'_sha256']=sha(path);receipt[name+'_path']=str(path);receipt[name+'_sha256']=sha(path)
    receipt_path=write(control/'receipt.json',receipt);proof.update(receipt=str(receipt_path),receipt_sha256=sha(receipt_path));doc['readiness_proofs']=proof
    source=write(OUT/'source.json',doc)
    checked=cli._validate_complex_wavelet_command(source,runs,allowances,True)
    assert checked['_execution_identity']==identity
    records_out.append({"name":"constructed-source-schema", "test_fixture":True, "passed":True})

    # Reused physical arrays are explicitly constructed guard fixtures, not candidate rows.
    image=OUT/'image.tif';runner.gff.write_u16(image,np.zeros((512,512,3),np.uint16))
    label=OUT/'label.tif';runner.gff.write_u16(label,np.zeros((512,512,1),np.uint16))
    support=OUT/'support.npy';np.save(support,np.ones((512,512),bool))
    plane={'path':str(label),'sha256':sha(label)};live={'path':str(support),'sha256':sha(support),'pixels':512*512}
    map_tree=[[dict(plane) for _ in range(3)] for _ in range(5)]
    records=[]
    for config in complex_wavelet.CONFIGURATIONS:
     for key in keys:
        records.append({**key,'configuration_id':config['id'],'repeat':0,'input_identity':{'fixture':'a'*64},'coverage_identity':{'coverage':None},'aligned_identity':None,'whole':str(image),'whole_sha256':sha(image),'tiled':str(image),'tiled_sha256':sha(image),
          **{name:copy.deepcopy(map_tree) for name in ('coefficient_source_maps','tiled_coefficient_source_maps','raw_coefficient_source_maps','tiled_raw_coefficient_source_maps')},
          'support_maps':[[dict(live) for _ in range(3)] for _ in range(5)],'tiled_support_maps':[[dict(live) for _ in range(3)] for _ in range(5)],
          'pixel_source_map':dict(plane) if config['colour']=='reassign' else None,'tiled_pixel_source_map':dict(plane) if config['colour']=='reassign' else None,
          'orientation_label_maps':[dict(plane,orientation=b,support_path=str(support),support_sha256=sha(support)) for b in range(3)],
          'native_whole_tile':dict(different_samples=0,pixel_source_differences=0,orientation_map_differences=[0]*15,raw_map_differences=[0]*15,support_differences=[0]*15)})
    runner.complex_wavelet_repeat_preflight(records)
    records_out.append({"name":"constructed-repeat-native-files", "test_fixture":True, "passed":True})
    for mutation in ('unknown-key','missing-map','wrong-hash','missing-file','typed-key'):
        bad=copy.deepcopy(records)
        if mutation=='unknown-key':bad[0]['seed']='111,211,311'
        if mutation=='typed-key':bad[0]['tile']=[]
        if mutation=='missing-map':bad[0].pop('raw_coefficient_source_maps')
        if mutation=='wrong-hash':bad[0]['whole_sha256']='b'*64
        if mutation=='missing-file':bad[0]['whole']=str(OUT/'absent.tif')
        try:runner.complex_wavelet_repeat_preflight(bad)
        except ValueError:records_out.append({"name":"repeat-"+mutation,"test_fixture":True,"refused":True})
        else:raise AssertionError(mutation)

    base=next(r for r in details if r['family']=='thin-hairs' and r['seed']=='101,201,301' and r['tile']==512)
    pool=[{'path':str(p),'sha256':sha(p)} for p in runner._selection_source(base).paths]
    resource_rows=[]
    for count in (8,32):
     for repeat in range(3):
        directory=OUT/f'body-n{count}-r{repeat}'
        payload={'test_fixture':True,'registration_sha256':sha(source),'execution_identity':identity,'body':{'frame_count':count,'configuration_id':'shared-raw','input_shape_hwc':[512,512,3],'physical_input_pool':pool,'declared_references':[p['path'] for p in pool]*(count//8),'read_counts':[2]*count,'composite':str(image),'composite_sha256':sha(image),'state_bound':{'phase_total_bytes':{'constructed_fixture':100}}}}
        argv=native(directory,payload)
        row={'frame_count':count,'repeat':repeat,'native_exit':0,'argv':argv}
        for path_key,hash_key,file in [('argv_path','argv_sha256','argv.json'),('body_receipt','body_receipt_sha256','body.json'),('stdout','stdout_sha256','stdout.txt'),('stderr','stderr_sha256','stderr.txt'),('time_v','time_v_sha256','time.txt'),('exit_path','exit_sha256','exit.txt')]:
            row[path_key]=str(directory/file);row[hash_key]=sha(directory/file)
        resource_rows.append(row)
    artifact_doc={'test_fixture':True,'body_count':6,'registration_sha256':sha(source),'execution_identity':identity,'records':resource_rows}
    artifact=write(OUT/'resources.json',artifact_doc)
    audit=write(OUT/'audit.json',dict(test_fixture=True,passed=True,resource_body_count=6,source_lifetime_pass=True,native_receipts_pass=True,execution_identity=identity,source_registration=str(source.resolve()),source_registration_sha256=sha(source),resource_artifact=str(artifact.resolve()),resource_artifact_sha256=sha(artifact)))
    quality_doc=copy.deepcopy(doc);quality_doc.update(quality_ready=True,resources=dict(source_registration=str(source),source_registration_sha256=sha(source),resource_artifact=str(artifact),resource_artifact_sha256=sha(artifact),audit_receipt=str(audit),audit_receipt_sha256=sha(audit),execution_identity=identity))
    quality=write(OUT/'quality.json',quality_doc)
    cli._validate_complex_wavelet_command(quality,runs,allowances,False)
    records_out.append({"name":"constructed-quality-schema", "test_fixture":True, "passed":True})
    for mutation in ('empty','duplicate-key','missing-argv','duplicate-pid'):
        bad=copy.deepcopy(artifact_doc)
        if mutation=='empty':bad={}
        if mutation=='duplicate-key':bad['records'][-1]=bad['records'][0]
        if mutation=='missing-argv':bad['records'][0]['argv_path']=str(OUT/'absent.json')
        if mutation=='duplicate-pid':
            receipt2=json.loads(Path(bad['records'][1]['body_receipt']).read_text());receipt2['pid']=json.loads(Path(bad['records'][0]['body_receipt']).read_text())['pid'];p=write(OUT/'duplicate-pid.json',receipt2);bad['records'][1].update(body_receipt=str(p),body_receipt_sha256=sha(p))
        try:cli._validate_complex_wavelet_resources(bad,identity,sha(source),set())
        except ValueError:records_out.append({"name":"resource-"+mutation,"test_fixture":True,"refused":True})
        else:raise AssertionError(mutation)
    for mutation in ('OR','runtime','baseline','missing-assessor','missing-proof-file','stale-identity','training-coverage','fixture-workload'):
        bad=copy.deepcopy(doc)
        if mutation=='OR':bad['benefit_conjunction']='OR'
        if mutation=='runtime':bad['runtime']['python']='0.0'
        if mutation=='baseline':bad['baseline_sha256']='a'*64
        if mutation=='missing-assessor':bad['metric_assessor']['path']=str(OUT/'absent.py')
        if mutation=='missing-proof-file':bad['readiness_proofs']['stdout_path']=str(OUT/'absent.txt')
        if mutation=='stale-identity':bad['execution_identity']='a'*64
        if mutation=='training-coverage':bad['training_coverage']=[]
        path=write(OUT/(mutation+'.json'),bad)
        output=OUT/('refused-'+mutation)
        expected={'OR':'settled immutable benefit', 'runtime':'immutable semantics', 'baseline':'changed immutable baseline', 'missing-assessor':'file', 'missing-proof-file':'stdout file', 'stale-identity':'execution identity is stale', 'training-coverage':'training coverage is missing or stale', 'fixture-workload':'constructed validation fixtures cannot authorize measured workloads'}[mutation]
        diagnostic=io.StringIO()
        with contextlib.redirect_stderr(diagnostic):
            try:cli.main(['complex-wavelet','--resource-only','--registration',str(path),'--runs',str(runs),'--allowances',str(allowances),'--out',str(output)])
            except SystemExit as error:assert error.code!=0
            else:raise AssertionError(mutation)
        message=diagnostic.getvalue();print(message, file=sys.stderr, end='')
        assert expected in message, (mutation,expected,message)
        assert not output.exists()
        records_out.append({"name":"public-"+mutation,"expected_error":expected,"test_fixture":True,"refused":True})
    def refuse(name, expected, action, absent=None):
        captured = io.StringIO()
        with contextlib.redirect_stderr(captured):
            try:
                returned = action()
            except (ValueError, SystemExit) as error:
                if isinstance(error, SystemExit):
                    assert error.code != 0, name
                message = str(error) + captured.getvalue()
            else:
                assert returned == 1, f"{name}: refusal escaped"
                message = captured.getvalue()
        assert expected in message, (name, expected, message)
        assert absent is None or not absent.exists(), name
        records_out.append({"name": name, "expected_error": expected, "diagnostic": message, "test_fixture": True, "refused": True})

    for name, expected in (("stale-source", "source bindings are stale"), ("stale-dependency", "source bindings are stale"),
                           ("missing-readiness", "not source-ready"), ("missing-proof", "valid source correctness receipt")):
        bad = copy.deepcopy(doc)
        if name == "stale-source":
            bad['execution_module_sha256'][bad['canonical_modules'][0]] = '0' * 64
        elif name == "stale-dependency":
            bad['dependency_sha256'][next(iter(bad['dependency_sha256']))] = '0' * 64
        elif name == "missing-readiness":
            bad['execution_ready'] = False
        else:
            bad['readiness_proofs'] = {}
        path = write(OUT / (name + '.json'), bad)
        refuse(name, expected, lambda path=path: _validate_complex_wavelet_command(path, runs, allowances, True))
    refuse("source-only-quality", "immutable QUALITY_READY", lambda: _validate_complex_wavelet_command(source, runs, allowances, False))
    foreign_runs = OUT / "changed-baseline"
    foreign_runs.mkdir()
    write(foreign_runs / "baseline.json", [])
    refuse("changed-baseline-bytes", "baseline records do not match", lambda: _validate_complex_wavelet_command(source, foreign_runs, allowances, True))
    changed_allowance = write(OUT / "changed-allowance.json", [])
    refuse("changed-allowance-bytes", "allowances do not match", lambda: _validate_complex_wavelet_command(source, runs, changed_allowance, True))
    for name in ("missing-resource", "changed-resource", "missing-audit", "malformed-audit", "malformed-state"):
        bad = copy.deepcopy(quality_doc)
        if name == "missing-resource":
            bad['resources']['resource_artifact'] = str(OUT / 'absent-resource.json')
        elif name == "changed-resource":
            bad['resources']['resource_artifact_sha256'] = '0' * 64
        elif name == "missing-audit":
            bad['resources']['audit_receipt'] = str(OUT / 'absent-audit.json')
        elif name == "malformed-audit":
            path = write(OUT / 'malformed-audit-body.json', [])
            bad['resources'].update(audit_receipt=str(path), audit_receipt_sha256=sha(path))
        else:
            changed = copy.deepcopy(artifact_doc)
            body_path = Path(changed['records'][0]['body_receipt'])
            body = json.loads(body_path.read_text())
            body['body']['state_bound'] = []
            path = write(OUT / 'malformed-state-body.json', body)
            changed['records'][0].update(body_receipt=str(path), body_receipt_sha256=sha(path))
            artifact_path = write(OUT / 'malformed-state-artifact.json', changed)
            audit_body = json.loads(audit.read_text())
            audit_body.update(resource_artifact=str(artifact_path.resolve()), resource_artifact_sha256=sha(artifact_path))
            audit_path = write(OUT / 'malformed-state-audit.json', audit_body)
            bad['resources'].update(resource_artifact=str(artifact_path), resource_artifact_sha256=sha(artifact_path), audit_receipt=str(audit_path), audit_receipt_sha256=sha(audit_path))
        path = write(OUT / (name + '.json'), bad)
        expected = 'source lifetime state is invalid' if name == 'malformed-state' else 'source registration or audit is invalid' if name == 'malformed-audit' else 'resource proof or audit is missing or stale'
        refuse(name, expected, lambda path=path: _validate_complex_wavelet_command(path, runs, allowances, False))
    unknown = copy.deepcopy(records)
    unknown[0]['seed'] = '111,211,311'
    refuse('sealed-seed', 'exact unique 240 training keys', lambda: runner.complex_wavelet_repeat_preflight(unknown))
    real_output = OUT / 'refused-unset-environment'
    old_environment = os.environ.pop('FOCUSFORGE_REFERENCE_STACK', None)
    try:
        refuse('unset-real-environment', 'FOCUSFORGE_REFERENCE_STACK', lambda: main(['complex-wavelet', '--source', 'stack', '--registration', str(quality), '--runs', str(runs), '--allowances', str(allowances), '--out', str(real_output)]), real_output)
    finally:
        if old_environment is not None:
            os.environ['FOCUSFORGE_REFERENCE_STACK'] = old_environment
    # Small immutable physical inputs exercise preparation failures before any crop.
    pool_paths = [Path(bound['path']) for bound in pool]
    raw = [{'source_path': str(path.resolve()), 'sha256': sha(path), **runner._header(path)} for path in pool_paths]
    preparation = {'source': 'stack', 'raw_inputs': raw, 'alignment_exit': 1, 'strict': False, 'settings': runner.ALIGN_SETTINGS, 'reference_frame': 3}
    bypass = write(OUT / 'bypassed-preparation.json', preparation)
    refuse('missing-preparation', 'manifest is required', lambda: runner._validate_preparation(OUT / 'absent-preparation.json', 'stack', pool_paths))
    refuse('bypassed-preparation', 'strict alignment exit 0', lambda: runner._validate_preparation(bypass, 'stack', pool_paths))
    preparation.update(alignment_exit=0, strict=True, alignment_report=str(OUT / 'absent-report.json'))
    stale = write(OUT / 'stale-preparation.json', preparation)
    refuse('stale-preparation-report', 'alignment report is missing or stale', lambda: runner._validate_preparation(stale, 'stack', pool_paths))
    refuse('changed-preparation-bytes', 'native file is missing or stale', lambda: _wavelet_verify_file(str(stale), doc['real']['manifest_sha256'], set()))
    write(OUT / 'summary.json', {'test_fixture': True, 'scientific_executions': 0, 'execution_ready_for_science': False, 'records': records_out})
    return records_out


def _mask_pyramid_semantics() -> dict[str, object]:
    """Return every immutable arm setting required before an output exists."""
    return {
        "arm": "mask-pyramid", "method": "laplacian", "levels": 5, "score_radii": [8, 4, 2, 1, 1], "equalize": False,
        "shared_protocol_sha256": "e6faafb9539343a1579ca71f9d2cdac00b8a7395f8c64a05ec720eaaa7f54ab6",
        "hypothesis": "Hard finest Details selection with Gaussian coarse masks reduces edge/halo error against Details without losing thin-hair contrast or violating flat-noise, chroma, coverage or other objective floors.",
        "configurations": [{"id": "soft-coarse", "classification": "candidate", "can_advance": True, "mask_rule": "gaussian"},
                           {"id": "hard-every-level", "classification": "control", "can_advance": False, "mask_rule": "hard"},
                           {"id": "uniform-weights", "classification": "control", "can_advance": False, "mask_rule": "uniform"}],
        "training": {"holdout": "sealed", "seeds": ["101,201,301", "102,202,302", "103,203,303", "104,204,304", "105,205,305"], "tiles": [512, 1024]},
        "mask": {"coarse": "five-tap clamp-edge Gaussian reductions including residual", "eligibility": "unchanged Details per-level erosion including residual",
                 "epsilon": float(np.finfo(np.float32).eps), "fallback": "eligible weight sum below float32 epsilon uses plain eligible-source coefficient mean",
                 "finest": "strict-greater current Details L0; first eligible lowest index wins ties; one map across channels",
                 "hard_control": "unchanged non-advancing ablation: direct phase-aligned decimation of immutable finest one-hot mask",
                 "partition_bound": "gamma_(2N+8)=(2N+8)*epsilon/(1-(2N+8)*epsilon); independently sum normalized weights; N tested 1,2,8,32",
                 "residual": "mask-weighted; differs from Details eligible residual mean by design", "uncovered": "zero coefficients and cleared output; explicit uncovered support",
                 "uniform_control": "equal eligible-source weights at every level"},
        "tile": {"details_reach_pixels": 140, "margin_pixels": 160, "phase_pixels": 32, "probe": "masked odd non-square ramp/impulse; nine interior windows",
                 "probe_shape_hw": [545, 547], "probe_tile": 512, "reach_pixels": 136, "training_metric": "actual candidate whole/tile pair only; unavailable null plus reason",
                 "whole_tile_image_lsb": 1, "whole_tile_labels_lsb": 0},
        "detectors": {"failure": "missing, nonfinite, stale or insensitive fixed-positive proof forces defer; no retuning",
                      "hard-every-level": {"role": "unchanged non-advancing ablation", "family": "colour-edges", "member": "default", "keys": "reported original hard 0/10; no advancement claim", "loss": "discrimination report only"},
                      "uniform-weights": {"family": "thin-hairs", "member": "default", "keys": "all five training seeds times tiles512/1024", "loss": "hair_retention loss exceeds exact Details keyed allowance on every designated key"}},
        "advancement": {"benefit": "mean edge/halo benefit against Details exceeds two keyed allowances with thin-hair nonregression", "chroma_and_invalid_border": "unchanged keyed allowances and absolute invalid-source contamination rule",
                        "flat_noise": "Details floor within unchanged keyed allowance; Weighted Average gap report-only", "hard_veto": "any objective per-seed loss above one exact keyed allowance is no-go",
                        "missing_evidence": "defer", "negative_controls": "discrimination only; their deliberate losses do not directly veto soft-coarse"},
        "output_matrix": {"training_input_keys": 60, "configurations_per_key": 3, "training_records": 180, "repeat_records": 180, "details_references": 60,
                          "weighted_references": 60, "depth_map_context": 60, "gff_context_max_calls": 61, "gff_scale": 1.0, "real_records": 3, "real_montages": 1},
        "budget": {"sessions": 2, "seconds_per_session": 7200, "total_seconds": 14400,
                   "include": "failed checks, replays, context, resources, repeats, verification, gates and report work", "exclude": "actual recorded lock wait"},
        "real": {"source": "stack", "raw_indices": [0, 8, 17, 26, 34, 43, 52, 60], "reference_ordinal": 3, "rect_xywh": [1878, 1056, 1024, 1024],
                 "processing_origin_xy": [0, 0], "aligned_origin_modulo32": [22, 0], "common_covered_pixels": 1048576,
                 "prepared_manifest_sha256": "c61d135c8ca1e25d7d53b6b3db1bc5d7c9d1cbad1bd9e5d660a65b10f88ec405", "alignment_report_sha256": "bc4ca98f0d6bb5e980ace9c4ae57fd5ee5ecc03c5c99623144fb947438aa9ce3",
                 "unset_environment": "explicit skip without preparation or output", "truth": "none; diagnostic only; no uncropped equivalence claim"},
    }


def _mask_pyramid_execution_identity(document: dict[str, object], modules: dict[str, str], dependencies: dict[str, str]) -> str:
    """Hash immutable scientific, source, and input bindings across readiness transitions."""
    resources = document["resources"]
    resource_procedure = {key: resources[key] for key in ("cache", "comparison", "frame_counts", "fresh_processes_per_count", "noise_procedure", "source")}
    payload = {"semantics": _mask_pyramid_semantics(), "modules": modules, "dependencies": dependencies,
               "baseline_sha256": document["baseline_sha256"], "allowances_sha256": document["allowances_sha256"],
               "resource_procedure": resource_procedure, "positive_proof": document["positive_proof"],
               "original_reference_noise": resources["original_reference_noise"]}
    return hashlib.sha256(json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def _validate_mask_pyramid_command(registration_path: Path, runs: Path, allowances: Path, source: str | None,
                                   prepared: Path | None, rect: str | None, resource_only: bool) -> dict[str, object]:
    """Reject unbound mask-pyramid execution before it creates an output."""
    if not registration_path.is_file() or not (runs / "baseline.json").is_file() or not allowances.is_file():
        raise ValueError("--registration, --runs baseline.json, and --allowances are required")
    try:
        document = json.loads(registration_path.read_text())
    except json.JSONDecodeError as error:
        raise ValueError("registration is not valid JSON") from error
    for key, value in _mask_pyramid_semantics().items():
        if document.get(key) != value:
            raise ValueError(f"registration {key} does not match the immutable mask-pyramid protocol")
    expected = ["__init__.py", "__main__.py", "protocol.py", "scenes.py", "metrics.py", "fusion.py", "runner.py", "focus_score.py", "index_field.py", "consistency.py", "coefficient_shrinkage.py", "mask_pyramid.py"]
    package = Path(__file__).parent
    names = [f"spikes/stacking_experiments/{name}" for name in expected]
    if document.get("canonical_modules") != names:
        raise ValueError("registration canonical module set is invalid")
    current = {name: hashlib.sha256((package / Path(name).name).read_bytes()).hexdigest() for name in names}
    if document.get("execution_module_sha256") != current:
        raise ValueError("registration does not bind the current mask-pyramid implementation")
    root = package.parents[1]
    dependencies = document.get("dependency_sha256")
    expected_dependencies = ("spikes/guided-filter-fusion/gff_fusion.py", "spikes/synthetic-ground-truth/synth_stack.py",
                             "tools/lib/rawtiff.py", "tools/tiff-diff.py")
    if not isinstance(dependencies, dict) or tuple(sorted(dependencies)) != tuple(sorted(expected_dependencies)):
        raise ValueError("registration dependency bindings are invalid")
    dependency_current: dict[str, str] = {}
    for name in dependencies:
        path = root / name
        if not path.is_file():
            raise ValueError("registration dependency is missing")
        dependency_current[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    if dependencies != dependency_current:
        raise ValueError("registration dependencies do not match the current implementation")
    if hashlib.sha256((runs / "baseline.json").read_bytes()).hexdigest() != document.get("baseline_sha256"):
        raise ValueError("baseline records do not match the registration")
    focus_score._validate_baseline_inputs(runs / "baseline.json")
    if hashlib.sha256(allowances.read_bytes()).hexdigest() != document.get("allowances_sha256"):
        raise ValueError("allowances do not match the registration")
    if document.get("execution_ready") is not True:
        raise ValueError("registration is not source-ready")
    resources = document.get("resources")
    if not isinstance(resources, dict):
        raise ValueError("registration resources are invalid")
    resource_static = {"cache": "OS page cache uncontrolled; no cold/warm cache claim", "comparison": "each repaired-source N median RSS must differ from the repaired-source N=2 median by at most the immutable original 884KiB bound; current-run N=2 noise is report-only",
                       "frame_counts": [2, 8, 16, 32], "fresh_processes_per_count": 3,
                       "noise_procedure": "max minus min peak RSS KiB of the three N=2 fresh process bodies",
                       "source": "file-backed references to the same eight physical files; no N-sized pixel arrays"}
    if any(resources.get(key) != value for key, value in resource_static.items()):
        raise ValueError("registration resource procedure does not match the immutable mask-pyramid protocol")
    original_noise = resources.get("original_reference_noise")
    if (not isinstance(original_noise, dict) or set(original_noise) != {"artifact", "artifact_sha256", "n2_noise_kb"}
            or not isinstance(original_noise["artifact"], str) or not isinstance(original_noise["artifact_sha256"], str)
            or len(original_noise["artifact_sha256"]) != 64 or original_noise["n2_noise_kb"] != 884):
        raise ValueError("registration does not bind the immutable original-reference noise proof")
    original_artifact = Path(original_noise["artifact"])
    if not original_artifact.is_file() or hashlib.sha256(original_artifact.read_bytes()).hexdigest() != original_noise["artifact_sha256"]:
        raise ValueError("immutable original-reference noise artifact is stale")
    try:
        original_data = json.loads(original_artifact.read_text())
        original_records = original_data["records"]
        original_n2 = [int(row["peak_rss_kb"]) for row in original_records if row["frame_count"] == 2]
        original_source = Path(str(original_data["source_registration"]))
        original_source_sha256 = original_data["source_registration_sha256"]
        original_source_document = json.loads(original_source.read_text())
    except (KeyError, TypeError, ValueError, OSError, json.JSONDecodeError) as error:
        raise ValueError("immutable original-reference noise artifact is malformed") from error
    original_groups = [[row for row in original_records if isinstance(row, dict) and row.get("frame_count") == count]
                       for count in (2, 8, 16, 32)]
    original_pids = [row.get("process_id") for group in original_groups for row in group]
    if (len(original_records) != 12 or any(len(group) != 3 or {row.get("repeat") for row in group} != {0, 1, 2}
                                           for group in original_groups)
            or any(not isinstance(pid, int) or pid <= 0 for pid in original_pids) or len(set(original_pids)) != 12
            or not original_source.is_file() or hashlib.sha256(original_source.read_bytes()).hexdigest() != original_source_sha256
            or original_source_document.get("execution_ready") is not True or original_source_document.get("quality_ready") is not False
            or original_data.get("baseline_sha256") != document.get("baseline_sha256")
            or original_data.get("allowances_sha256") != document.get("allowances_sha256")
            or original_source_document.get("baseline_sha256") != document.get("baseline_sha256")
            or original_source_document.get("allowances_sha256") != document.get("allowances_sha256")
            or original_source_document.get("execution_identity") != original_data.get("execution_identity")
            or len(original_n2) != 3 or max(original_n2) - min(original_n2) != 884 or original_data.get("n2_noise_kb") != 884):
        raise ValueError("immutable original-reference noise proof does not reproduce 884KiB")
    positive = document.get("positive_proof")
    positive_required = {"artifact", "artifact_sha256", "registration", "registration_sha256", "helper_path", "helper_sha256",
                         "input_sha256", "allowances_sha256", "support_sha256", "strengths", "maxima", "source_bindings",
                         "dependency_bindings"}
    hash_fields = {"artifact_sha256", "registration_sha256", "helper_sha256", "input_sha256", "allowances_sha256"}
    valid_hash = lambda value: isinstance(value, str) and len(value) == 64 and all(character in "0123456789abcdef" for character in value)
    if (not isinstance(positive, dict) or set(positive) != positive_required
            or any(not valid_hash(positive[key]) for key in hash_fields)
            or any(not isinstance(positive[key], str) for key in ("artifact", "registration", "helper_path"))
            or not isinstance(positive["support_sha256"], dict) or set(positive["support_sha256"]) != {"halo", "seam"}
            or any(not valid_hash(value) for value in positive["support_sha256"].values())
            or positive["strengths"] != {"halo": 617, "seam": 248}
            or positive["maxima"] != {"edge_halo": 308.44566241918585, "seam_energy": 123.76985677083341}
            or any(isinstance(value, bool) or not isinstance(value, int) for value in positive["strengths"].values())
            or any(isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(float(value))
                   for value in positive["maxima"].values())
            or not isinstance(positive["source_bindings"], dict) or set(positive["source_bindings"]) != set(names)
            or any(not valid_hash(value) for value in positive["source_bindings"].values())
            or not isinstance(positive["dependency_bindings"], dict) or set(positive["dependency_bindings"]) != set(expected_dependencies)
            or any(not valid_hash(value) for value in positive["dependency_bindings"].values())):
        raise ValueError("registration does not bind the fixed positive detector proof")
    positive_artifact = Path(positive["artifact"])
    positive_registration = Path(positive["registration"])
    helper = Path(positive["helper_path"])
    if (not positive_artifact.is_file() or hashlib.sha256(positive_artifact.read_bytes()).hexdigest() != positive["artifact_sha256"]
            or not positive_registration.is_file() or hashlib.sha256(positive_registration.read_bytes()).hexdigest() != positive["registration_sha256"]
            or not helper.is_file() or hashlib.sha256(helper.read_bytes()).hexdigest() != positive["helper_sha256"]
            or positive["allowances_sha256"] != document["allowances_sha256"]):
        raise ValueError("fixed positive detector proof is stale")
    repaired_modules = {"spikes/stacking_experiments/__main__.py", "spikes/stacking_experiments/mask_pyramid.py"}
    if (positive["dependency_bindings"] != dependency_current
            or any(positive["source_bindings"][name] != current[name] for name in names if name not in repaired_modules)):
        raise ValueError("fixed positive detector proof does not bind the unchanged current sources")
    try:
        receipt = json.loads(positive_artifact.read_text())
        positive_registration_document = json.loads(positive_registration.read_text())
        native = receipt["tiffs"]
        receipt_hashes = {key: hashlib.sha256((positive_artifact.parent / f"{key}.tif").read_bytes()).hexdigest() for key in native}
    except (KeyError, TypeError, ValueError, OSError, json.JSONDecodeError) as error:
        raise ValueError("fixed positive detector receipt is malformed") from error
    values = receipt.get("values", {})
    expected_effects = {name: float(value) for name, value in positive["strengths"].items()}
    registered_helper = Path(str(positive_registration_document.get("helper_path", "")))
    registered_truth = Path(str(positive_registration_document.get("truth_path", "")))
    if not registered_helper.is_absolute():
        registered_helper = root / registered_helper
    if not registered_truth.is_absolute():
        registered_truth = root / registered_truth
    registered_case = registered_truth.with_name("case.json")
    metric_sha256 = hashlib.sha256(json.dumps(values, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    if (not isinstance(receipt, dict) or not isinstance(positive_registration_document, dict)
            or not isinstance(native, dict) or set(native) != {"clean", "halo", "seam"}
            or any(not isinstance(item, dict) for item in native.values())
            or not isinstance(values, dict) or any(not isinstance(values.get(name), dict) for name in native)
            or receipt.get("native_exit") != 0 or receipt.get("registration_sha256") != positive["registration_sha256"]
            or receipt.get("helper_sha256") != positive["helper_sha256"] or receipt.get("truth_sha256") != positive["input_sha256"]
            or receipt.get("allowances_sha256") != positive["allowances_sha256"]
            or receipt.get("support_sha256") != positive["support_sha256"] or receipt.get("strengths") != positive["strengths"]
            or receipt.get("maxima") != positive["maxima"] or receipt.get("source_bindings") != positive["source_bindings"]
            or receipt.get("dependency_bindings") != positive["dependency_bindings"]
            or registered_helper.resolve() != helper.resolve()
            or not registered_truth.is_file() or hashlib.sha256(registered_truth.read_bytes()).hexdigest() != positive["input_sha256"]
            or receipt.get("case_sha256") != positive_registration_document.get("case_sha256")
            or not registered_case.is_file() or hashlib.sha256(registered_case.read_bytes()).hexdigest() != receipt.get("case_sha256")
            or positive_registration_document.get("helper_sha256") != positive["helper_sha256"]
            or positive_registration_document.get("truth_sha256") != positive["input_sha256"]
            or positive_registration_document.get("allowances_sha256") != positive["allowances_sha256"]
            or positive_registration_document.get("support_sha256") != positive["support_sha256"]
            or positive_registration_document.get("support_pixels") != receipt.get("support_pixels")
            or positive_registration_document.get("strengths") != positive["strengths"]
            or positive_registration_document.get("maxima") != positive["maxima"]
            or positive_registration_document.get("execution_modules") != positive["source_bindings"]
            or positive_registration_document.get("dependencies") != positive["dependency_bindings"]
            or values.get("clean", {}).get("edge_halo") != 0.0 or values.get("clean", {}).get("seam_energy") != 0.0
            or values.get("halo", {}).get("edge_halo") != expected_effects["halo"]
            or values.get("seam", {}).get("seam_energy") != expected_effects["seam"]
            or receipt.get("effects") != expected_effects
            or receipt.get("metric_sha256") != metric_sha256
            or receipt["effects"]["halo"] <= positive["maxima"]["edge_halo"]
            or receipt["effects"]["seam"] <= positive["maxima"]["seam_energy"]
            or native["clean"].get("sha256") != positive["input_sha256"]
            or any(receipt_hashes[name] != native[name].get("sha256") for name in native)):
        raise ValueError("fixed positive detector receipt does not authenticate its native proof")
    document["_validated_registration_sha256"] = hashlib.sha256(registration_path.read_bytes()).hexdigest()
    document["_execution_identity"] = _mask_pyramid_execution_identity(document, current, dependency_current)
    if not resource_only:
        artifact = Path(str(resources.get("noise_artifact", "")))
        if document.get("quality_ready") is not True or resources.get("resource_guard_completed") is not True or not artifact.is_file():
            raise ValueError("registration is not quality-ready")
        if hashlib.sha256(artifact.read_bytes()).hexdigest() != resources.get("noise_artifact_sha256"):
            raise ValueError("resource noise artifact is stale")
        source_registration = Path(str(resources.get("source_registration", "")))
        if not source_registration.is_file() or hashlib.sha256(source_registration.read_bytes()).hexdigest() != resources.get("source_registration_sha256"):
            raise ValueError("source-ready registration provenance is missing or stale")
        try:
            source_document = json.loads(source_registration.read_text())
        except json.JSONDecodeError as error:
            raise ValueError("source-ready registration provenance is not valid JSON") from error
        source_bindings = {"canonical_modules": document["canonical_modules"], "execution_module_sha256": document["execution_module_sha256"],
                           "dependency_sha256": document["dependency_sha256"], "baseline_sha256": document["baseline_sha256"],
                           "allowances_sha256": document["allowances_sha256"], "positive_proof": document["positive_proof"]}
        if (not isinstance(source_document, dict) or source_document.get("execution_ready") is not True or source_document.get("quality_ready") is not False
                or any(source_document.get(key) != value for key, value in source_bindings.items())
                or any(source_document.get(key) != value for key, value in _mask_pyramid_semantics().items())
                or not isinstance(source_document.get("resources"), dict)
                or any(source_document["resources"].get(key) != value for key, value in resource_static.items())
                or source_document["resources"].get("original_reference_noise") != resources["original_reference_noise"]
                or _mask_pyramid_execution_identity(source_document, current, dependency_current) != document["_execution_identity"]):
            raise ValueError("source-ready registration provenance does not bind this execution")
        try:
            measured = json.loads(artifact.read_text())
        except json.JSONDecodeError as error:
            raise ValueError("resource noise artifact is not valid JSON") from error
        records = measured.get("records") if isinstance(measured, dict) else None
        if not isinstance(records, list) or len(records) != 12 or {item.get("frame_count") for item in records if isinstance(item, dict)} != {2, 8, 16, 32}:
            raise ValueError("resource noise artifact does not contain the measured twelve bodies")
        bindings = {"execution_module_sha256": document["execution_module_sha256"], "dependency_sha256": document["dependency_sha256"],
                    "baseline_sha256": document["baseline_sha256"], "allowances_sha256": document["allowances_sha256"],
                    "execution_identity": document["_execution_identity"], "source_registration_sha256": resources["source_registration_sha256"],
                    "source_registration": str(source_registration.resolve()), "positive_proof": document["positive_proof"],
                    "original_reference_noise": resources["original_reference_noise"]}
        if any(measured.get(key) != value for key, value in bindings.items()):
            raise ValueError("resource noise artifact does not bind the registered sources and inputs")
        current_mask_sha = current["spikes/stacking_experiments/mask_pyramid.py"]
        baseline_rows = json.loads((runs / "baseline.json").read_text())
        resource_row = next((item for item in baseline_rows if item.get("family") == "flat-noise" and item.get("member") == "noise-250"
                             and item.get("method") == "laplacian" and item.get("tile") == 512 and item.get("repeat") == 0), None)
        if not isinstance(resource_row, dict):
            raise ValueError("resource noise artifact has no registered retained input")
        pool = sorted((Path(str(resource_row["case"])) / "frames").glob("*.tif"))
        fingerprints = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in pool}
        if len(pool) != 8:
            raise ValueError("resource noise artifact input pool is incomplete")
        valid_counts = []
        pids: set[int] = set()
        for count in (2, 8, 16, 32):
            group = [item for item in records if isinstance(item, dict) and item.get("frame_count") == count]
            if len(group) != 3 or {item.get("repeat") for item in group} != {0, 1, 2}:
                raise ValueError("resource noise artifact does not contain three bodies per frame count")
            for item in group:
                pid = item.get("process_id")
                if not isinstance(pid, int) or pid <= 0 or pid in pids:
                    raise ValueError("resource noise artifact bodies are not independent processes")
                pids.add(pid)
                expected_refs = [str(path) for path in (pool * ((count + 7) // 8))[:count]]
                numeric = (item.get("sample_reads") == 2 * count and item.get("mask_reads", 0) + item.get("generated_default_masks", 0) == 2 * count
                           and isinstance(item.get("peak_rss_kb"), int) and item["peak_rss_kb"] > 0
                           and isinstance(item.get("body_wall_seconds"), (int, float)) and math.isfinite(float(item["body_wall_seconds"])) and float(item["body_wall_seconds"]) > 0)
                if (item.get("physical_input_pool_files") != 8 or item.get("distinct_referenced_files") != min(count, 8)
                        or item.get("declared_references") != expected_refs or item.get("input_fingerprints") != fingerprints
                        or item.get("source_module_sha256") != current_mask_sha or not numeric):
                    raise ValueError("resource noise artifact has incomplete or stale body evidence")
                paths = {key: Path(str(item.get(key, ""))) for key in ("stdout", "stderr", "time_v", "argv_path", "exit_path")}
                hashes = {"stdout": "stdout_sha256", "stderr": "stderr_sha256", "time_v": "time_v_sha256", "argv_path": "argv_sha256", "exit_path": "exit_sha256"}
                if any(not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != item.get(hashes[key]) for key, path in paths.items()):
                    raise ValueError("resource noise artifact raw body evidence is missing or stale")
                if paths["exit_path"].read_text() != "0\n" or item.get("native_exit") != 0:
                    raise ValueError("resource noise artifact body native exit is invalid")
                try:
                    raw_argv = json.loads(paths["argv_path"].read_text())
                    raw_body = json.loads(paths["stdout"].read_text())
                except json.JSONDecodeError as error:
                    raise ValueError("resource noise artifact raw command or body is not valid JSON") from error
                if raw_argv != item.get("argv") or not isinstance(raw_body, dict):
                    raise ValueError("resource noise artifact raw command or body disagrees with its record")
                if raw_argv != mask_pyramid_resource_argv(runs, count, paths["time_v"]):
                    raise ValueError("resource noise artifact probe invocation is invalid")
                body_keys = ("frame_count", "process_id", "physical_input_pool_files", "distinct_referenced_files", "declared_references", "sample_reads", "mask_reads", "generated_default_masks", "peak_rss_kb", "composite_sha256", "input_fingerprints", "source_module_sha256", "cache_limitation")
                if any(raw_body.get(key) != item.get(key) for key in body_keys):
                    raise ValueError("resource noise artifact raw body does not match its record")
                time_text = paths["time_v"].read_text()
                rss = re.search(r"Maximum resident set size \(kbytes\): (\d+)", time_text)
                cpu = re.findall(r"(?:User|System) time \(seconds\): ([0-9.]+)", time_text)
                elapsed = re.search(r"Elapsed \(wall clock\) time \(h:mm:ss or m:ss\): ([0-9:.]+)", time_text)
                if "Exit status: 0" not in time_text or rss is None or len(cpu) != 2 or elapsed is None:
                    raise ValueError("resource noise artifact GNU time evidence is incomplete")
                try:
                    elapsed_parts = [float(value) for value in elapsed.group(1).split(":")]
                    if len(elapsed_parts) == 2:
                        elapsed_seconds = elapsed_parts[0] * 60 + elapsed_parts[1]
                    elif len(elapsed_parts) == 3:
                        elapsed_seconds = elapsed_parts[0] * 3600 + elapsed_parts[1] * 60 + elapsed_parts[2]
                    else:
                        raise ValueError
                except ValueError as error:
                    raise ValueError("resource noise artifact GNU elapsed time is invalid") from error
                body_seconds = float(item["body_wall_seconds"])
                if (int(rss.group(1)) <= 0 or int(item["peak_rss_kb"]) > int(rss.group(1)) or not math.isfinite(elapsed_seconds)
                        or elapsed_seconds <= 0 or abs(body_seconds - elapsed_seconds) > 1.0
                        or any(not math.isfinite(float(value)) or float(value) < 0 for value in cpu)):
                    raise ValueError("resource noise artifact GNU time does not agree with its body record")
            valid_counts.append(group)
        n2 = [item["peak_rss_kb"] for item in valid_counts[0]]
        noise = max(n2) - min(n2)
        medians = {str(count): sorted(item["peak_rss_kb"] for item in group)[1] for count, group in zip((2, 8, 16, 32), valid_counts)}
        comparisons = {count: {"median_rss_kb": median, "difference_from_n2_kb": abs(median - medians["2"]), "within_n2_noise": abs(median - medians["2"]) <= noise} for count, median in medians.items()}
        if measured.get("n2_noise_kb") != noise or measured.get("median_rss_kb") != medians or measured.get("comparison") != comparisons:
            raise ValueError("resource noise artifact summaries do not match its measured bodies")
        original_flatness = all(item["difference_from_n2_kb"] <= original_noise["n2_noise_kb"] for item in comparisons.values())
        original_comparisons = {count: {"median_rss_kb": median, "difference_from_n2_kb": abs(median - medians["2"]),
                                        "within_original_noise": abs(median - medians["2"]) <= original_noise["n2_noise_kb"]}
                                for count, median in medians.items()}
        if (measured.get("original_reference_noise") != original_noise or measured.get("original_reference_comparison") != original_comparisons
                or measured.get("original_reference_flatness_pass") is not all(item["within_original_noise"] for item in original_comparisons.values())
                or measured.get("resource_guard_completed") is not True or not original_flatness):
            raise ValueError("resource guard completion or flatness summary is inconsistent")
    if source == "mantis":
        raise ValueError("--source mantis is not registered")
    if source is None:
        if prepared is not None or rect is not None:
            raise ValueError("--prepared and --rect require --source stack")
    elif source != "stack" or prepared is None or rect != "1878,1056,1024,1024":
        raise ValueError("--source stack requires the registered preparation manifest and rectangle")
    else:
        _validate_preparation(prepared, source)
        if hashlib.sha256(prepared.read_bytes()).hexdigest() != document["real"]["prepared_manifest_sha256"]:
            raise ValueError("preparation manifest does not match the registered real input")
    return document


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


def _photometric_calibration_documents(readiness: dict[str, object], registration_sha256: str) -> tuple[dict[str, object], dict[str, object], dict[str, dict[str, str]]]:
    """Load the two calibration records only from immutable readiness bindings."""
    hashes = readiness.get("input_hashes")
    if not isinstance(hashes, dict):
        raise ValueError("photometric readiness has no calibration bindings")
    documents: dict[str, dict[str, object]] = {}
    bindings: dict[str, dict[str, str]] = {}
    for raw_path, digest in hashes.items():
        if not isinstance(raw_path, str) or not isinstance(digest, str):
            continue
        path = Path(raw_path)
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError("photometric calibration binding is stale")
        if path.suffix != ".json":
            continue
        try:
            document = json.loads(path.read_text(), object_pairs_hook=photometric._unique_object)
        except json.JSONDecodeError as error:
            raise ValueError("photometric calibration binding is not JSON") from error
        if isinstance(document, dict) and document.get("registration_sha256") == registration_sha256 and document.get("control") in ("off", "global-gain"):
            control = str(document["control"])
            if control in documents:
                raise ValueError("photometric readiness has duplicate calibration records")
            documents[control] = document
            bindings[control] = {"path": str(path.resolve()), "sha256": digest}
    if set(documents) != {"off", "global-gain"}:
        raise ValueError("photometric readiness lacks both calibration records")
    photometric._quality_calibration(documents["off"], "off", registration_sha256)
    photometric._quality_calibration(documents["global-gain"], "global-gain", registration_sha256)
    photometric._quality_identity(documents["global-gain"])
    return documents["off"], documents["global-gain"], bindings


def _photometric_readiness(runs: Path, registration_sha256: str, stage: str) -> dict[str, object]:
    """Load the immutable per-command readiness admission from the runs directory."""
    return photometric.readiness_document(runs, registration_sha256, stage)


def _photometric_allowance_calibrations(envelope: dict[str, object], registration_sha256: str) -> dict[str, dict[str, str]] | None:
    if envelope.get("state") != "allowance-ready":
        return None
    readiness_path, _ = photometric._binding(envelope.get("readiness"), "photometric allowance readiness binding is invalid")
    readiness = photometric._json_document(readiness_path, "photometric allowance readiness is invalid")
    pairs = (("off", "calibration_off"), ("global-gain", "calibration_global"))
    bindings = {}
    for control, field in pairs:
        path, digest = photometric._binding(readiness.get(field), "photometric allowance calibration binding is invalid")
        document = photometric._json_document(path, "photometric allowance calibration is invalid")
        photometric._quality_calibration(document, control, registration_sha256)
        if control == "global-gain":
            photometric._quality_identity(document)
        bindings[control] = {"path": str(path.resolve()), "sha256": digest}
    return bindings


def _photometric_write_comparison(output: Path, name: str, value: object) -> None:
    path = output / "comparison.json"
    document = json.loads(path.read_text(), object_pairs_hook=photometric._unique_object)
    if not isinstance(document, dict):
        raise ValueError("photometric comparison is not an object")
    document[name] = value
    path.write_text(json.dumps(document, indent=2, sort_keys=True, allow_nan=False) + "\n")


def alignment_reference_dispatch_self_check() -> str:
    """Exercise the public alignment-reference argument path without fitting."""
    import contextlib
    import io
    import tempfile

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        captured: list[tuple[tuple[object, ...], dict[str, object]]] = []

        def executor(*values: object, **named: object) -> dict[str, str]:
            captured.append((values, named))
            return {"stage": str(values[0])}

        digest = "0" * 64
        result = main(["alignment-reference", "--stage", "quality", "--registration", str(root / "registration.json"),
                       "--source-proof", str(root / "source.json"), "--source-proof-sha256", digest,
                       "--parity-proof", str(root / "parity.json"), "--parity-proof-sha256", digest,
                       "--allowance-proof", str(root / "allowance.json"), "--allowance-proof-sha256", digest,
                       "--root", str(root), "--spike", str(root / "spike"), "--cli", str(root / "cli"),
                       "--quality-digest", digest, "--quality-records", str(root / "quality.json"),
                       "--calibration-records", str(root / "calibration.json"), "--calibration-records-sha256", digest,
                       "--real-manifest", str(root / "real.json"), "--out", str(root / "out")], alignment_executor=executor)
        if result != 0 or len(captured) != 1:
            raise AssertionError("alignment-reference CLI did not dispatch once")
        values, named = captured[0]
        if (values != ("quality", root / "registration.json", root / "source.json", root / "allowance.json", root / "parity.json",
                      root, root / "spike", root / "cli", root / "out")
                or named.get("source_proof_sha256") != digest or named.get("parity_proof_sha256") != digest
                or named.get("allowance_proof_sha256") != digest or named.get("quality_digest") != digest
                or named.get("quality_records") != root / "quality.json"
                or named.get("calibration_records") != root / "calibration.json"
                or named.get("calibration_records_sha256") != digest
                or named.get("real_manifest") != root / "real.json"):
            raise AssertionError("alignment-reference CLI lost immutable inputs")
        with contextlib.redirect_stderr(io.StringIO()):
            try:
                main(["alignment-reference", "--records", "ignored"], alignment_executor=executor)
            except SystemExit:
                pass
            else:
                raise AssertionError("obsolete alignment-reference records input accepted")
            try:
                main(["alignment-reference", "--stage", "quality"], alignment_executor=executor)
            except SystemExit:
                pass
            else:
                raise AssertionError("incomplete alignment-reference inputs accepted")
        if len(captured) != 1:
            raise AssertionError("invalid alignment-reference input reached executor")
    return "cli-dispatch"


def main(argv: list[str] | None = None, *, alignment_executor: object | None = None) -> int:
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
    alignment_reference_command = sub.add_parser("alignment-reference")
    alignment_reference_command.add_argument("--self-check", dest="alignment_reference_self_check", action="store_true")
    alignment_reference_command.add_argument("--stage", choices=alignment_reference.STAGES)
    alignment_reference_command.add_argument("--registration")
    alignment_reference_command.add_argument("--source-proof")
    alignment_reference_command.add_argument("--source-proof-sha256")
    alignment_reference_command.add_argument("--parity-proof")
    alignment_reference_command.add_argument("--parity-proof-sha256")
    alignment_reference_command.add_argument("--allowance-proof")
    alignment_reference_command.add_argument("--allowance-proof-sha256")
    alignment_reference_command.add_argument("--calibration-records")
    alignment_reference_command.add_argument("--calibration-records-sha256")
    alignment_reference_command.add_argument("--parity-records")
    alignment_reference_command.add_argument("--parity-records-sha256")
    alignment_reference_command.add_argument("--verify-parity", action="store_true")
    alignment_reference_command.add_argument("--quality-records")
    alignment_reference_command.add_argument("--root")
    alignment_reference_command.add_argument("--spike")
    alignment_reference_command.add_argument("--cli")
    alignment_reference_command.add_argument("--quality-digest")
    alignment_reference_command.add_argument("--real-manifest")
    alignment_reference_command.add_argument("--out")
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
    noise_score_command = sub.add_parser("noise-score")
    noise_score_command.add_argument("--self-check", dest="noise_score_self_check", action="store_true")
    noise_score_sub = noise_score_command.add_subparsers(dest="noise_score_command")
    noise_cases = noise_score_sub.add_parser("cases")
    noise_cases.add_argument("--seed", required=True)
    noise_cases.add_argument("--frames", type=int, choices=noise_score.FRAME_COUNTS)
    noise_cases.add_argument("--allow-holdout", action="store_true")
    noise_cases.add_argument("--out", required=True)
    noise_baseline = noise_score_sub.add_parser("baseline")
    noise_baseline.add_argument("--split", choices=("training",), required=True)
    noise_baseline.add_argument("--out", required=True)
    noise_hashes = noise_score_sub.add_parser("fusion-hashes")
    noise_hashes.add_argument("--out", required=True)
    noise_checkpoint = noise_score_sub.add_parser("checkpoint")
    noise_checkpoint.add_argument("--registration", required=True)
    noise_checkpoint.add_argument("--runs", required=True)
    noise_checkpoint.add_argument("--out", required=True)
    noise_arms = noise_score_sub.add_parser("arms")
    noise_arms.add_argument("--split", choices=("training",), required=True)
    noise_arms.add_argument("--registration", required=True)
    noise_arms.add_argument("--checkpoint", required=True)
    noise_arms.add_argument("--runs", required=True)
    noise_arms.add_argument("--out", required=True)
    noise_arms.add_argument("--replay-only", action="store_true")
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
    masks = sub.add_parser("mask-pyramid")
    masks.add_argument("--self-check", dest="mask_pyramid_self_check", action="store_true")
    masks.add_argument("--split", choices=("training",), default="training")
    masks.add_argument("--registration")
    masks.add_argument("--runs")
    masks.add_argument("--allowances")
    masks.add_argument("--out")
    masks.add_argument("--source", choices=("stack", "mantis"))
    masks.add_argument("--prepared")
    masks.add_argument("--rect")
    masks.add_argument("--resource-only", action="store_true")
    masks.add_argument("--repeat-only", action="store_true")
    wavelet = sub.add_parser("complex-wavelet")
    wavelet.add_argument("--self-check", dest="wavelet_self_check", action="store_true")
    wavelet.add_argument("--reach-only", action="store_true")
    wavelet.add_argument("--break", dest="wavelet_break", choices=("missing-conjugate", "ignore-mask", "per-channel-selection", "real-only-magnitude", "inconsistent-vote-gather"))
    wavelet.add_argument("--registration")
    wavelet.add_argument("--runs")
    wavelet.add_argument("--allowances")
    wavelet.add_argument("--out")
    wavelet.add_argument("--split", choices=("training",))
    wavelet.add_argument("--source", choices=("stack",))
    wavelet.add_argument("--prepared")
    wavelet.add_argument("--rect")
    wavelet.add_argument("--resource-only", action="store_true")
    wavelet.add_argument("--training-sha256", help="immutable first comparison SHA256 required for repeat")
    wavelet.add_argument("--resource-body", type=int, choices=(8, 32), help=argparse.SUPPRESS)
    wavelet.add_argument("--repeat-only", action="store_true")
    photo = sub.add_parser("photometric")
    photo.add_argument("--self-check", dest="photometric_self_check", action="store_true")
    photo.add_argument("--source-controls", action="store_true")
    photo.add_argument("--parity-only", action="store_true")
    photo.add_argument("--calibration-only", action="store_true")
    photo.add_argument("--control", choices=("off", "global-gain"))
    photo.add_argument("--split", choices=("training",))
    photo.add_argument("--repeat-only", action="store_true")
    photo.add_argument("--training")
    photo.add_argument("--training-sha256")
    photo.add_argument("--registration")
    photo.add_argument("--runs")
    photo.add_argument("--out")
    photo.add_argument("--prepared")
    photo.add_argument("--source", choices=("stack",))
    photo.add_argument("--rect")
    args = parser.parse_args(argv)
    if args.self_check:
        return self_check(Path(args.allowances) if args.allowances else None)
    if args.command == "noise-score":
        if args.noise_score_self_check:
            try:
                for line in noise_score.self_checks():
                    print(line)
            except (AssertionError, ValueError) as error:
                print(str(error), file=sys.stderr)
                return 1
            return 0
        if args.noise_score_command == "cases":
            try:
                seed = SeedTriple.parse(args.seed)
                case_split(seed, args.allow_holdout)
            except ValueError as error:
                parser.error(str(error))
            counts = noise_score.FRAME_COUNTS if args.frames is None else (args.frames,)
            document = noise_score.render_cases(Path(args.out), seed, counts, args.allow_holdout)
            print(f"{document['split']} cases: {len(document['cases'])}")
            return 0
        if args.noise_score_command == "baseline":
            rows = run_baselines(Path(args.out), TRAINING_SEEDS, members=noise_score.BASELINE_MEMBERS)
            print(f"baseline records: {len(rows)}")
            return 0
        if args.noise_score_command == "fusion-hashes":
            out = Path(args.out)
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text(json.dumps(noise_score.fusion_reference_hashes(), indent=2, sort_keys=True) + "\n")
            return 0
        if args.noise_score_command == "checkpoint":
            try:
                document = noise_score.write_checkpoint(Path(args.registration), Path(args.runs), Path(args.out))
            except ValueError as error:
                parser.error(str(error))
            print(f"checkpoint configurations: {len(document['configurations'])}")
            return 0
        if args.noise_score_command == "arms":
            try:
                comparison = noise_score.run_arms(Path(args.registration), Path(args.checkpoint), Path(args.runs), Path(args.out),
                                                  args.replay_only, sys.argv if argv is None else [sys.argv[0], *argv])
            except ValueError as error:
                parser.error(str(error))
            print(f"replay records: {len(comparison['replay'])}")
            print(f"candidate rows: {len(comparison['candidate_rows'])}")
            return 0
        noise_score_command.print_help()
        return 2
    if args.command == "alignment-reference":
        executor = alignment_reference.execute_registered_stage if alignment_executor is None else alignment_executor
        if not callable(executor):
            raise TypeError("alignment executor must be callable")
        if args.verify_parity:
            if any(value is None for value in (args.parity_records, args.parity_records_sha256, args.calibration_records, args.calibration_records_sha256)):
                parser.error("parity verification requires pinned --parity-records and --calibration-records")
            if any(value is not None for value in (args.stage, args.registration, args.source_proof, args.parity_proof, args.allowance_proof, args.root, args.spike, args.cli, args.quality_records, args.real_manifest, args.out)):
                parser.error("parity verification accepts only parity and calibration records")
            try:
                parity = alignment_reference.verify_parity_records(Path(args.parity_records), args.parity_records_sha256, Path(args.calibration_records), args.calibration_records_sha256)
            except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
                parser.error(str(error))
            for row in parity["comparisons"]:
                print(f"alignment-reference parity {row['strategy']} repeat={row['repeat']} frame={row['frame']} corner={json.dumps(row['corner_difference'], sort_keys=True)} tolerance={json.dumps(row['tau'])}")
            print("alignment-reference parity: ok")
            return 0
        if args.alignment_reference_self_check:
            if any((args.stage, args.registration, args.source_proof, args.source_proof_sha256, args.parity_proof, args.parity_proof_sha256, args.allowance_proof, args.allowance_proof_sha256, args.calibration_records, args.calibration_records_sha256, args.parity_records, args.parity_records_sha256, args.quality_records, args.root, args.spike, args.cli, args.quality_digest, args.real_manifest, args.out)):
                parser.error("alignment-reference self-check does not accept stage inputs")
            try:
                for name in [*alignment_reference.self_checks(), alignment_reference_dispatch_self_check()]:
                    print(f"{name}: ok")
            except (AssertionError, ValueError) as error:
                print(str(error), file=sys.stderr)
                return 1
            return 0
        if any(value is None for value in (args.stage, args.registration, args.root, args.spike, args.cli, args.out)):
            parser.error("alignment-reference requires --stage, --registration, --root, --spike, --cli, and --out")
        try:
            record = executor(
                args.stage,
                Path(args.registration),
                Path(args.source_proof) if args.source_proof else None,
                Path(args.allowance_proof) if args.allowance_proof else None,
                Path(args.parity_proof) if args.parity_proof else None,
                Path(args.root), Path(args.spike), Path(args.cli), Path(args.out),
                source_proof_sha256=args.source_proof_sha256,
                parity_proof_sha256=args.parity_proof_sha256,
                allowance_proof_sha256=args.allowance_proof_sha256,
                quality_digest=args.quality_digest,
                real_manifest=Path(args.real_manifest) if args.real_manifest else None,
                calibration_records=Path(args.calibration_records) if args.calibration_records else None,
                calibration_records_sha256=args.calibration_records_sha256,
                quality_records=Path(args.quality_records) if args.quality_records else None,
            )
        except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
            parser.error(str(error))
        print(f"alignment-reference stage: {record['stage']}")
        return 0
    if args.command == "registration":
        document = json.dumps(registration(), indent=2, sort_keys=True)
        if args.out:
            Path(args.out).write_text(document + "\n")
        print(document)
        return 0
    if args.command == "photometric":
        if args.photometric_self_check:
            if any((args.source_controls, args.parity_only, args.calibration_only, args.repeat_only, args.split, args.source, args.prepared, args.training, args.training_sha256, args.rect)):
                parser.error("photometric self-check does not accept measured inputs")
            if (args.registration is None) != (args.runs is None):
                parser.error("photometric self-check requires --registration and --runs together")
            output = Path(args.out or ".scratch/photometric/controls")
            try:
                if args.registration is not None:
                    photometric.registration_document(Path(args.registration))
                for record in [*photometric.control_records(output), *photometric_physical_controls(output / "physical")]:
                    print(f"{record['name']}: ok {json.dumps(record, sort_keys=True)}")
            except (AssertionError, ValueError) as error:
                print(str(error), file=sys.stderr)
                return 1
            return 0
        if args.source_controls:
            if any((args.parity_only, args.calibration_only, args.repeat_only, args.split, args.source, args.prepared, args.registration, args.runs, args.training, args.training_sha256, args.rect)):
                parser.error("photometric source controls do not accept real input")
            if args.out is None:
                parser.error("photometric source controls require --out")
            try:
                records = run_photometric_source_controls(Path(args.out))
            except (RuntimeError, ValueError) as error:
                print(str(error), file=sys.stderr)
                return 1
            print(f"photometric source controls: {len(records)}")
            return 0
        selected = sum(bool(value) for value in (args.parity_only, args.calibration_only, args.repeat_only, args.split, args.source))
        if selected != 1:
            parser.error("photometric requires exactly one registered measured mode")
        if not all((args.registration, args.runs, args.out)):
            parser.error("photometric measured modes require --registration, --runs, and --out")
        if args.parity_only:
            stage = "parity"
            if any((args.control, args.training, args.training_sha256, args.prepared, args.rect)):
                parser.error("photometric parity does not accept calibration, repeat, or real inputs")
        elif args.calibration_only:
            if args.control is None:
                parser.error("photometric calibration requires --control")
            stage = "calibration-off" if args.control == "off" else "calibration-global"
            if any((args.training, args.training_sha256, args.prepared, args.rect)):
                parser.error("photometric calibration does not accept training or real inputs")
        elif args.repeat_only:
            stage = "repeat"
            if not all((args.training, args.training_sha256)) or any((args.control, args.prepared, args.rect)):
                parser.error("photometric repeat requires --training and --training-sha256 only")
        elif args.source:
            stage = "real"
            if args.source != "stack" or args.prepared is None or args.rect is None or any((args.control, args.training, args.training_sha256)):
                parser.error("photometric real requires --source stack --prepared and --rect")
        else:
            stage = "quality"
            if args.split != "training" or any((args.control, args.training, args.training_sha256, args.prepared, args.rect)):
                parser.error("photometric quality requires --split training")
        try:
            if stage == "parity" or stage.startswith("calibration") or stage in ("quality", "repeat", "real"):
                document, digest, envelope = photometric.stage_envelope(Path(args.registration), stage)
                readiness = _photometric_readiness(Path(args.runs), digest, stage)
            else:
                document, digest = photometric.registration_document(Path(args.registration))
                envelope = None
                readiness = photometric.readiness_document(Path(args.runs), digest, stage)
            output = Path(args.out)
            if output.exists():
                raise ValueError("photometric output must not exist before readiness admission")
            if stage == "repeat":
                training = photometric_repeat_preflight(Path(args.training), str(args.training_sha256), digest)
                if readiness.get("training_sha256") != args.training_sha256:
                    raise ValueError("photometric repeat training comparison is missing or stale")
            else:
                training = None
            expected_calibrations = _photometric_allowance_calibrations(envelope, digest) if envelope is not None else None
            calibrations = None
            if stage in ("quality", "repeat", "real"):
                calibrations = _photometric_calibration_documents(readiness, digest)
                if expected_calibrations is None or calibrations[2] != expected_calibrations:
                    raise ValueError("photometric readiness calibration bindings differ from allowance evidence")
            prepared, rect = None, None
            if stage == "real":
                rect = tuple(int(value) for value in args.rect.split(","))
                if rect != (1878, 1056, 1024, 1024):
                    raise ValueError("photometric real rectangle differs from registration")
                prepared = Path(args.prepared)
                full = photometric_full_frame_manifest(prepared, "stack")
                if document.get("original_inputs", {}).get("preparation", {}).get("sha256") != hashlib.sha256(prepared.read_bytes()).hexdigest() or not full["photometric_full_frame"]["coverage"]["common_valid_pixels"]:
                    raise ValueError("photometric real preparation is stale or has no full-frame coverage")
                photometric_real_roi_coverage(full["photometric_full_frame"], rect)
            records = run_photometric_stage(stage, output, Path(args.runs), readiness, prepared, args.source, rect)
            if stage == "quality":
                assert calibrations is not None
                off, global_gain, bindings = calibrations
                verdict = photometric.quality_verdict(records, off, global_gain, digest)
                if not isinstance(verdict, dict):
                    raise ValueError("photometric quality verdict is invalid")
                _photometric_write_comparison(output, "quality_verdict", verdict)
                _photometric_write_comparison(output, "calibration_bindings", bindings)
            elif stage == "repeat":
                assert training is not None and calibrations is not None
                repeated = json.loads((output / "comparison.json").read_text(), object_pairs_hook=photometric._unique_object)
                audit = photometric_repeat_equality(training, repeated, digest)
                _photometric_write_comparison(output, "repeat_audit", {**audit, "training_sha256": args.training_sha256, "reference_sha256": args.training_sha256,
                                                                         "calibration_bindings": calibrations[2]})
        except (AttributeError, OSError, ValueError, RuntimeError, json.JSONDecodeError) as error:
            parser.error(str(error))
        print(f"photometric {stage}: {len(records)} rows")
        return 0
    if args.command == "complex-wavelet":
        if args.training_sha256 is not None and not args.repeat_only:
            parser.error("--training-sha256 requires --repeat-only")
        if args.wavelet_break is not None and not (args.wavelet_self_check or args.reach_only):
            parser.error("--break requires a complex-wavelet control mode")
        if args.resource_body is not None and not args.resource_only:
            parser.error("--resource-body requires --resource-only")
        if (args.prepared is not None or args.rect is not None) and args.source != "stack":
            parser.error("--prepared and --rect require --source stack")
        if args.wavelet_self_check and args.reach_only:
            parser.error("--self-check and --reach-only are mutually exclusive")
        if args.wavelet_self_check or args.reach_only:
            context = (args.registration, args.runs, args.allowances)
            if args.split is not None:
                parser.error("complex-wavelet controls do not consume training splits")
            if any(context) and (not all(context) or args.reach_only):
                parser.error("complete --self-check context requires --registration, --runs, and --allowances together")
            if args.resource_only or args.repeat_only or args.source is not None:
                parser.error("complex-wavelet controls do not accept quality, repeat, resource, or real-source modes")
            out = Path(args.out or (".scratch/complex-wavelet/reach" if args.reach_only else ".scratch/complex-wavelet/controls"))
            try:
                records = complex_wavelet.self_checks(out, args.wavelet_break)
                if all(context) and args.wavelet_break is None:
                    records += complex_wavelet_case_controls(out)
                    records += _complex_wavelet_guard_checks(Path(args.registration), Path(args.runs), Path(args.allowances), out)
            except (AssertionError, ValueError) as error:
                print(str(error), file=sys.stderr)
                return 1
            if args.wavelet_break is not None:
                print(f"broken variant escaped detection: {args.wavelet_break}", file=sys.stderr)
                return 1
            for record in records:
                print(f"{record['name']}: ok {json.dumps(record, sort_keys=True)}")
            return 0
        if args.resource_only and args.repeat_only:
            parser.error("--resource-only and --repeat-only are mutually exclusive")
        if args.resource_only and args.source is not None:
            parser.error("--resource-only does not accept a real source")
        if args.repeat_only and args.source is not None:
            parser.error("--repeat-only does not accept a real source")
        if args.split is not None and (args.resource_only or args.repeat_only or args.source is not None):
            parser.error("--split is only valid for synthetic training mode")
        if not args.resource_only and not args.repeat_only and args.source is None and args.split != "training":
            parser.error("complex-wavelet synthetic quality mode requires --split training")
        if not all((args.registration, args.runs, args.allowances, args.out)):
            parser.error("--registration, --runs, --allowances, and --out are required")
        if args.source == "stack" and not os.environ.get("FOCUSFORGE_REFERENCE_STACK"):
            print("FOCUSFORGE_REFERENCE_STACK is required for complex-wavelet real mode", file=sys.stderr)
            return 1
        try:
            document = _validate_complex_wavelet_command(Path(args.registration), Path(args.runs), Path(args.allowances), args.resource_only)
        except ValueError as error:
            parser.error(str(error))
        if document.get("test_fixture") is True:
            parser.error("constructed validation fixtures cannot authorize measured workloads")
        if args.source == "stack":
            if args.prepared is None or args.rect != ",".join(str(v) for v in document["real"]["rect_xywh"]):
                parser.error("complex-wavelet real mode requires its registered preparation and exact crop")
            prepared = Path(args.prepared)
            try:
                _wavelet_verify_file(str(prepared), document["real"]["manifest_sha256"], set())
                manifest = _validate_preparation(prepared, "stack")
                if manifest.get("alignment_report_sha256") != document["real"]["report_sha256"]:
                    raise ValueError("complex-wavelet real alignment report differs from registration")
            except ValueError as error:
                parser.error(str(error))
        output = Path(args.out)
        repeat_reference = None
        if args.repeat_only:
            reference = Path(str(document.get("training_comparison", "")))
            if not reference.is_file():
                parser.error("complex-wavelet repeat requires immutable training_comparison record")
            try:
                if not isinstance(args.training_sha256, str) or hashlib.sha256(reference.read_bytes()).hexdigest() != args.training_sha256:
                    raise ValueError("immutable training comparison SHA256 is missing or changed")
                comparison = json.loads(reference.read_text())
                if not isinstance(comparison, dict):
                    raise ValueError("immutable training comparison must be a JSON object")
                if comparison.get("execution_identity") != document["_execution_identity"] or comparison.get("registration_sha256") != document["_validated_registration_sha256"] or comparison.get("repeat_only") is not False:
                    raise ValueError("immutable training comparison execution or registration identity differs")
                repeat_reference = comparison["candidate_rows"]
                complex_wavelet_repeat_preflight(repeat_reference, json.loads((Path(args.runs) / "baseline.json").read_text()))
            except (KeyError, ValueError, json.JSONDecodeError) as error:
                parser.error(f"complex-wavelet repeat validation failed: {error}")
        if args.resource_only:
            output.mkdir(parents=True, exist_ok=True)
            if args.resource_body is not None:
                body = run_complex_wavelet_resource_body(Path(args.runs), output, args.resource_body)
                (output / "resource-body.json").write_text(json.dumps({"command": sys.argv if argv is None else [sys.argv[0], *argv],
                    "registration_sha256": document["_validated_registration_sha256"], "execution_identity": document["_execution_identity"],
                    "pid": os.getpid(), "body": body}, indent=2, sort_keys=True) + "\n")
                print(f"resource body N={args.resource_body}")
                return 0
            records = []
            for count in (8, 32):
                for repeat in range(3):
                    body_dir = output / f"body-n{count}-r{repeat}"
                    body_dir.mkdir(parents=True, exist_ok=True)
                    stdout, stderr, time_v, exit_path, argv_path = (body_dir / "stdout.txt", body_dir / "stderr.txt", body_dir / "time-v.txt", body_dir / "exit.txt", body_dir / "argv.json")
                    child = [sys.executable, "-m", "spikes.stacking_experiments", "complex-wavelet", "--resource-only", "--registration", str(args.registration), "--runs", str(args.runs), "--allowances", str(args.allowances), "--out", str(body_dir), "--resource-body", str(count)]
                    timed = ["/usr/bin/time", "-v", "-o", str(time_v), *child]
                    argv_path.write_text(json.dumps(timed) + "\n")
                    with stdout.open("w") as out_handle, stderr.open("w") as err_handle:
                        process = subprocess.run(timed, cwd=Path(__file__).resolve().parents[2], env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}, stdout=out_handle, stderr=err_handle, text=True)
                    exit_path.write_text(f"{process.returncode}\n")
                    if process.returncode:
                        raise RuntimeError(f"complex-wavelet resource body N={count} repeat={repeat} exited {process.returncode}")
                    body_json = body_dir / "resource-body.json"
                    if not body_json.is_file():
                        raise RuntimeError("complex-wavelet resource body wrote no receipt")
                    records.append({"frame_count": count, "repeat": repeat, "argv": timed, "argv_path": str(argv_path), "argv_sha256": hashlib.sha256(argv_path.read_bytes()).hexdigest(), "native_exit": process.returncode,
                                    "body_receipt": str(body_json), "body_receipt_sha256": hashlib.sha256(body_json.read_bytes()).hexdigest(),
                                    "stdout": str(stdout), "stdout_sha256": hashlib.sha256(stdout.read_bytes()).hexdigest(), "stderr": str(stderr), "stderr_sha256": hashlib.sha256(stderr.read_bytes()).hexdigest(),
                                    "time_v": str(time_v), "time_v_sha256": hashlib.sha256(time_v.read_bytes()).hexdigest(), "exit_path": str(exit_path), "exit_sha256": hashlib.sha256(exit_path.read_bytes()).hexdigest()})
            (output / "resource-bodies.json").write_text(json.dumps({"registration_sha256": document["_validated_registration_sha256"], "execution_identity": document["_execution_identity"], "records": records, "body_count": len(records)}, indent=2, sort_keys=True) + "\n")
            print("resource bodies: 6")
            return 0
        rows = (run_complex_wavelet_stack(output, args.source, Path(args.prepared) if args.prepared else None, args.rect, document["configurations"])["rows"]
                if args.source == "stack" else run_complex_wavelet_configurations(Path(args.runs), output, document["configurations"], repeat=1 if args.repeat_only else 0))
        paired, equality = [], []
        if args.source is None:
            paired = complex_wavelet_paired_effects(Path(args.runs), rows)
            if args.repeat_only:
                try:
                    assert repeat_reference is not None
                    equality = complex_wavelet_repeat_equality(rows, repeat_reference)
                except (ValueError, AssertionError) as error:
                    parser.error(f"complex-wavelet repeat equality failed: {error}")
        (output / "comparison.json").write_text(json.dumps({"command": sys.argv if argv is None else [sys.argv[0], *argv],
            "registration_sha256": document["_validated_registration_sha256"], "execution_identity": document["_execution_identity"],
            "candidate_rows": rows, "paired_signed_effects": paired, "repeat_native_equality": equality, "repeat_only": args.repeat_only}, indent=2, sort_keys=True) + "\n")
        print(f"candidate rows: {len(rows)}")
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
    if args.command == "mask-pyramid":
        if args.mask_pyramid_self_check:
            for record in mask_pyramid.self_checks():
                print(f"{record['name']}: ok {json.dumps(record, sort_keys=True)}")
            return 0
        if not all((args.registration, args.runs, args.allowances, args.out)):
            parser.error("--registration, --runs, --allowances, and --out are required")
        if args.resource_only and args.repeat_only:
            parser.error("--resource-only and --repeat-only are mutually exclusive")
        if args.resource_only and args.source is not None:
            parser.error("--resource-only does not accept a real source")
        if args.repeat_only and args.source is not None:
            parser.error("--repeat-only does not accept a real source")
        if args.source == "stack" and not os.environ.get("FOCUSFORGE_REFERENCE_STACK"):
            print("FOCUSFORGE_REFERENCE_STACK: real work skipped")
            return 0
        try:
            document = _validate_mask_pyramid_command(Path(args.registration), Path(args.runs), Path(args.allowances), args.source,
                                                      Path(args.prepared) if args.prepared else None, args.rect, args.resource_only)
        except ValueError as error:
            parser.error(str(error))
        output = Path(args.out)
        output.mkdir(parents=True, exist_ok=True)
        started = time.monotonic()
        if args.resource_only:
            resources = mask_pyramid_resource_controls(Path(args.runs), output)
            reference = document["resources"]["original_reference_noise"]
            n2_median = resources["median_rss_kb"]["2"]
            original_comparison = {count: {"median_rss_kb": median, "difference_from_n2_kb": abs(median - n2_median), "within_original_noise": abs(median - n2_median) <= reference["n2_noise_kb"]} for count, median in resources["median_rss_kb"].items()}
            resources["original_reference_comparison"] = original_comparison
            resources["original_reference_flatness_pass"] = all(item["within_original_noise"] for item in original_comparison.values())
            resources.update({"execution_module_sha256": document["execution_module_sha256"],
                              "dependency_sha256": document["dependency_sha256"], "baseline_sha256": document["baseline_sha256"],
                              "allowances_sha256": document["allowances_sha256"],
                              "execution_identity": document["_execution_identity"],
                              "source_registration_sha256": document["_validated_registration_sha256"],
                              "source_registration": str(Path(args.registration).resolve()),
                              "positive_proof": document["positive_proof"],
                              "original_reference_noise": reference})
            resource_path = output / "resource-controls.json"
            resource_path.write_text(json.dumps(resources, indent=2, sort_keys=True) + "\n")
            payload = {"command": sys.argv if argv is None else [sys.argv[0], *argv],
                       "registration_sha256": document["_validated_registration_sha256"], "resource_controls": resources,
                       "resource_artifact": str(resource_path), "budget": {"measured_seconds": time.monotonic() - started}}
        elif args.source == "stack":
            stack = run_mask_pyramid_stack(output, args.source, Path(args.prepared), args.rect, document["configurations"])
            payload = {"command": sys.argv if argv is None else [sys.argv[0], *argv],
                       "registration_sha256": document["_validated_registration_sha256"], "stack": stack,
                       "budget": {"measured_seconds": time.monotonic() - started}}
        else:
            rows = run_mask_pyramid_configurations(Path(args.runs), output, document["configurations"], Path(args.allowances),
                                                    "repeat" if args.repeat_only else "training")
            payload = {"command": sys.argv if argv is None else [sys.argv[0], *argv],
                       "registration_sha256": document["_validated_registration_sha256"], "candidate_rows": rows,
                       "references": mask_pyramid_references(Path(args.runs)),
                       "gff_context": mask_pyramid_gff_context(Path(args.runs), output) if not args.repeat_only else [],
                       "budget": {"measured_seconds": time.monotonic() - started}}
        (output / "comparison.json").write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
        print(f"candidate rows: {len(payload.get('candidate_rows', payload.get('stack', {}).get('rows', [])))}")
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
