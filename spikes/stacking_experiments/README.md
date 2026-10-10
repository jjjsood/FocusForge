# Stack experiments

Run from the repository root:

```text
python3 -m spikes.stacking_experiments --self-check
python3 -m spikes.stacking_experiments registration --out .scratch/stacking-controls/registration/protocol.json
python3 -m spikes.stacking_experiments focus-score --split training --registration /path/to/protocol.json --runs .scratch/stacking-controls --allowances .scratch/stacking-controls/allowances.json --replay-only --out .scratch/focus-score/replay
python3 -m spikes.stacking_experiments index-field --split training --registration /path/to/index-field-registration.json --runs .scratch/stacking-controls --allowances .scratch/stacking-controls/allowances.json --replay-only --out .scratch/index-field/replay
python3 -m spikes.stacking_experiments selection-consistency --split training --registration /path/to/selection-consistency-registration.json --runs .scratch/stacking-controls --allowances .scratch/stacking-controls/allowances.json --replay-only --out .scratch/selection-consistency/replay
python3 -m spikes.stacking_experiments noise-shrinkage --self-check
python3 -m spikes.stacking_experiments noise-shrinkage --split training --registration /path/to/noise-shrinkage-registration.json --runs .scratch/stacking-controls --allowances .scratch/stacking-controls/allowances.json --out .scratch/noise-shrinkage/training
python3 -m spikes.stacking_experiments mask-pyramid --self-check
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

`index-field` validates its own execution registration before writing output. Its replay mode checks the retained current Depth Map rows; a normal training run records the one frozen flat-noise calibration, controls, field arrays, rounded labels, and image measurements. `--family` restricts a focused training run to one registered family.

`selection-consistency` uses immutable raw Details winner maps. Its streamed path reads frame samples twice and reads masks separately; its small all-frame oracle is limited to identity checks.

`noise-shrinkage` selects the same Details tuples before applying a global per-level threshold. Its zero multiplier is an exact bypass; nonzero controls are chosen only from the registered finite strength lists.

`mask-pyramid` uses the current Details finest winner map with Gaussian coarse masks. The hard-mask arm is an unchanged non-advancing ablation. Its source-ready registration must bind the fixed positive detector proof and immutable original-reference 884KiB noise proof; current-run N=2 noise remains report-only.

Run `mask-pyramid --self-check` before registration. After the source registration is bound, run `mask-pyramid --resource-only` once; its measured artifact must be bound into the quality registration before `mask-pyramid` training or `--repeat-only` runs. Use `--source stack --prepared <manifest> --rect 1878,1056,1024,1024` only with the registered aligned preparation for the three real outputs.

Mask-pyramid sequence:

```text
python3 -m spikes.stacking_experiments mask-pyramid --self-check
python3 -m spikes.stacking_experiments mask-pyramid --resource-only --registration /path/to/source-ready-mask-pyramid.json --runs /path/to/stacking-controls --allowances /path/to/allowances.json --out /path/to/mask-pyramid-resource
python3 -m spikes.stacking_experiments mask-pyramid --split training --registration /path/to/quality-ready-mask-pyramid.json --runs /path/to/stacking-controls --allowances /path/to/allowances.json --out /path/to/mask-pyramid-training
python3 -m spikes.stacking_experiments mask-pyramid --repeat-only --registration /path/to/quality-ready-mask-pyramid.json --runs /path/to/stacking-controls --allowances /path/to/allowances.json --out /path/to/mask-pyramid-repeat
FOCUSFORGE_REFERENCE_STACK=/path/to/frames python3 -m spikes.stacking_experiments mask-pyramid --source stack --prepared /path/to/preparation-manifest.json --rect 1878,1056,1024,1024 --registration /path/to/quality-ready-mask-pyramid.json --runs /path/to/stacking-controls --allowances /path/to/allowances.json --out /path/to/mask-pyramid-real
```

Archive source-ready registration unchanged before quality transition. Resource output binds this archive, its fixed source and input identities, and all twelve raw body records. A quality-ready registration names the measured artifact before training, repeat, or real output.
