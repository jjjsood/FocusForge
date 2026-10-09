# Stack experiments

Run from the repository root:

```text
python3 -m spikes.stacking_experiments --self-check
python3 -m spikes.stacking_experiments registration --out .scratch/stacking-controls/registration/protocol.json
python3 -m spikes.stacking_experiments focus-score --split training --registration /path/to/protocol.json --runs .scratch/stacking-controls --allowances .scratch/stacking-controls/allowances.json --replay-only --out .scratch/focus-score/replay
python3 -m spikes.stacking_experiments baseline --split training --out .scratch/stacking-controls
python3 -m spikes.stacking_experiments cases --seed 101,201,301 --out .scratch/stacking-controls
python3 -m spikes.stacking_experiments cases --seed 111,211,311 --out .scratch/sealed-check
python3 -m spikes.stacking_experiments cases --seed 111,211,311 --allow-holdout --out .scratch/sealed-check
python3 -m spikes.stacking_experiments detectors --allowances .scratch/stacking-controls/allowances.json --out .scratch/stacking-controls/detectors.json
python3 -m spikes.stacking_experiments twins --runs .scratch/stacking-controls --out .scratch/stacking-controls/twins.json
FOCUSFORGE_REFERENCE_STACK=/path/to/frames python3 -m spikes.stacking_experiments prepare-real --source stack --out .scratch/stacking-controls/prepared/capture
FOCUSFORGE_REFERENCE_STACK=/path/to/frames python3 -m spikes.stacking_experiments crops --source stack --prepared .scratch/stacking-controls/prepared/capture/preparation-manifest.json --rect 1878,1056,1024,1024 --out .scratch/stacking-crops/capture
```

The hold-out command without `--allow-holdout` intentionally exits nonzero before rendering. The explicit flag is reserved for final synthesis. `FOCUSFORGE_REFERENCE_STACK` names the directory containing the 61 reduced mantis frames.

`prepare-real` aligns the registered full-resolution files with CPU pyramidal ECC, rotation-scale mode, an anchor reference and the middle frame. It writes a strict preparation manifest binding source headers and hashes, report, transforms, statuses, warped TIFFs, command exit, wall time and RSS. `crops` requires that manifest, verifies it again, proves all sources cover the registered aligned-reference rectangle, then reads only that rectangle from the aligned TIFFs. Synthetic co-registered cases use their registered no-alignment geometry.

The registration command records six scene families, three independent seed axes, metrics, allowance procedure, and advancement rules. CPU baseline runs record host RSS; VRAM is recorded as not used. Cold and warm labels are used only when measured.

`focus-score` validates the frozen comparison registration and inputs before creating output. A bound execution hash is required before a replay or comparison can run.
