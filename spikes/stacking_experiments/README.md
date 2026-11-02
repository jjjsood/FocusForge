# Stack experiments

Run from the repository root:

```text
python3 -m spikes.stacking_experiments --self-check
python3 -m spikes.stacking_experiments registration --out .scratch/stacking-controls/registration/protocol.json
python3 -m spikes.stacking_experiments focus-score --split training --registration /path/to/protocol.json --runs .scratch/stacking-controls --allowances .scratch/stacking-controls/allowances.json --replay-only --out .scratch/focus-score/replay
python3 -m spikes.stacking_experiments index-field --split training --registration /path/to/index-field-registration.json --runs .scratch/stacking-controls --allowances .scratch/stacking-controls/allowances.json --replay-only --out .scratch/index-field/replay
python3 -m spikes.stacking_experiments selection-consistency --split training --registration /path/to/selection-consistency-registration.json --runs .scratch/stacking-controls --allowances .scratch/stacking-controls/allowances.json --replay-only --out .scratch/selection-consistency/replay
python3 -m spikes.stacking_experiments noise-shrinkage --self-check
python3 -m spikes.stacking_experiments noise-score --self-check
python3 -m spikes.stacking_experiments noise-score cases --seed 101,201,301 --out .scratch/noise-score-cases
python3 -m spikes.stacking_experiments noise-score fusion-hashes --out .scratch/noise-score/fusion-hashes.json
python3 -m spikes.stacking_experiments noise-score baseline --split training --out .scratch/noise-score-baseline
python3 -m spikes.stacking_experiments noise-score checkpoint --registration /path/to/report.md --runs .scratch/noise-score-baseline --out /path/to/execution-checkpoint.json
python3 -m spikes.stacking_experiments noise-score arms --split training --registration /path/to/report.md --checkpoint /path/to/execution-checkpoint.json --runs .scratch/noise-score-baseline --replay-only --out .scratch/noise-score-arms/replay
python3 -m spikes.stacking_experiments photometric --self-check --out .scratch/photometric/controls
python3 -m spikes.stacking_experiments photometric --source-controls --out .scratch/photometric/source-controls
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

`photometric --self-check` proves fixed encoded-domain fitting and full-frame tile application controls. `--source-controls` runs the four current-product gain controls once and retains each command, report, native exit and GNU-time record. Measured modes require a registration, run directory, empty output directory and matching immutable readiness record: `--parity-only`, `--calibration-only --control off|global-gain`, `--split training`, `--repeat-only --training ... --training-sha256 ...`, or `--source stack --prepared ... --rect 1878,1056,1024,1024`. The command validates all hashes, the fixed row matrix and remaining budget before creating output. Real input is fit and corrected over full aligned TIFFs before the registered ROI is read.

The run directory supplies immutable `<stage>-admission.json` records under `photometric/`; calibration also requires `parity-ready.json`. A real invocation has the form:

```text
FOCUSFORGE_REFERENCE_STACK=/path/to/frames python3 -m spikes.stacking_experiments photometric --source stack --prepared /path/to/preparation-manifest.json --rect 1878,1056,1024,1024 --registration /path/to/allowance-ready.json --runs /path/to/runs --out /path/to/real
```

The admission record bounds one complete command invocation. The caller may enforce its wall-time limit around that command with a ten-second termination grace; the command does not create nested deadlines. Real output records full-frame fits, crop coverage, native receipts and labelled off/candidate diagnostic panels. All truth-dependent metrics remain unavailable because real inputs have no oracle image.

`prepare-real` aligns the registered full-resolution files with CPU pyramidal ECC, rotation-scale mode, an anchor reference and the middle frame. It writes a strict preparation manifest binding source headers and hashes, report, transforms, statuses, warped TIFFs, command exit, wall time and RSS. `crops` requires that manifest, verifies it again, proves all sources cover the registered aligned-reference rectangle, then reads only that rectangle from the aligned TIFFs. Synthetic co-registered cases use their registered no-alignment geometry.

The registration command records six scene families, three independent seed axes, metrics, allowance procedure, and advancement rules. CPU baseline runs record host RSS; VRAM is recorded as not used. Cold and warm labels are used only when measured.

`focus-score` validates the frozen comparison registration and inputs before creating output. A bound execution hash is required before a replay or comparison can run.

`index-field` validates its own execution registration before writing output. Its replay mode checks the retained current Depth Map rows; a normal training run records the one frozen flat-noise calibration, controls, field arrays, rounded labels, and image measurements. `--family` restricts a focused training run to one registered family.

`selection-consistency` uses immutable raw Details winner maps. Its streamed path reads frame samples twice and reads masks separately; its small all-frame oracle is limited to identity checks.

`noise-score` holds four candidate per-pixel focus measures (a stack noise floor, the Laplacian on a smoothed copy, and two ring differences) with their constants. `--self-check` verifies each against hand-computed values on small planes and checks that short-stack renders are deterministic and that the eight-frame renders keep their recorded frame hashes. `cases` writes the thin-hair and flat-noise members at 3, 5 and 8 frames (`--frames` picks one) with the share of hair pixels whose depth offset the shorter range clips; a sealed seed needs `--allow-holdout`. `baseline` runs the three current methods over those nine members (540 runs). `fusion-hashes` writes composite and winner-map hashes of the unmodified score replays, one eight-frame case per member.

`checkpoint` binds the registration section (the text from `### Registration` to the next `## ` heading), the baseline and allowance records, the arm constants, the twelve configuration ids and the SHA-256 of every rig module into one JSON file. `arms` refuses, before it creates `--out`, a missing or altered registration, a checkpoint whose module hashes differ from the live files, an unregistered configuration id, a changed constant, and baseline or allowance records that differ from the checkpoint. It then replays the current score over all 270 repeat-0 baseline rows and requires identical composites and Z-maps; `--replay-only` stops there. Otherwise it runs the twelve arm configurations (each of Details, Weighted Average and Depth map with each arm) at the recorded tile plans widened by the arm's added reach, rounded up to the pyramid alignment. Each candidate row carries the registration and checkpoint hashes. `comparison.json` also holds the pixels the arm-1 floor clears per member, seed and method, and each arm's measured score reach beside the current score's.

`noise-shrinkage` selects the same Details tuples before applying a global per-level threshold. Its zero multiplier is an exact bypass; nonzero controls are chosen only from the registered finite strength lists.

`mask-pyramid` uses the current Details finest winner map with Gaussian coarse masks. The hard-mask arm is an unchanged non-advancing ablation. Its source-ready registration must bind the fixed positive detector proof and immutable original-reference 884KiB noise proof; current-run N=2 noise remains report-only.

`complex-wavelet` implements the Lina--Mayrand symmetric six-tap complex Daubechies transform from [Lina and Mayrand (1995)](https://doi.org/10.1006/acha.1995.1015), using the [focus-stack filter transcription](https://github.com/PetteriAimonen/focus-stack/blob/0cd289e224fe24aead5b146faeaa46950775f346/src/task_wavelet_templates.hh) under its [MIT licence](https://github.com/PetteriAimonen/focus-stack/blob/0cd289e224fe24aead5b146faeaa46950775f346/LICENSE.md) as a numerical cross-check. Its low-pass taps are `sqrt(2)/64 * [-3, 5, 30, 30, 5, -3] + i*sqrt(30)/64 * [-1, -1, 2, 2, -1, -1]`; high-pass taps alternate conjugate signs. Analysis uses periodic `2k+j-3` samples and synthesis uses conjugate-adjoint taps. Inputs reflect-pad at the bottom/right to a 32-pixel multiple, while padded samples stay ineligible.

Use unwindowed summed channel `|c|²` scores with first-eligible strict ties. The grey control uses the arithmetic RGB mean and assigns output colour from the nearest eligible source grey value. Three orientation winner maps remain separate from this pixel reassignment map. Tile windows preserve the global padded periodic domain; the reach control records the required 32-phase margin.

The public resource command starts six fresh GNU-time child bodies: three each for eight and 32 references from one physical 512px RGB16 TIFF pool. It stores child argv, PID, stdout, stderr, exit, GNU time, physical read counters, composite, vote counts, structural-state evidence and receipt hashes. Training and repeat records additionally retain raw/final/support/reassignment maps. The internal child selector is not a public recipe. Resource records state a conservative structural ndarray upper bound separately from observed GNU RSS; OS page cache and NumPy C temporaries remain outside that bound. Their only N-dependent Python state is path references, TIFF metadata and reader counters.

The full self-check takes all three registration, runs and allowances inputs and uses the prospective registration as an immutable template for constructed native schemas and named preparation/registration refusals. These test fixtures cannot authorize measured workloads. Omitting all three inputs runs transform controls only; a partial context is rejected before output.

Training writes one whole TIFF and one global-periodic tiled TIFF per retained key, source/support hashes, all 17 frozen metric fields and three separate finest-orientation label metrics. Repeat requires the immutable training comparison record and rejects any native image, coefficient-map, support-map or reassignment-map mismatch.

```text
python3 -m spikes.stacking_experiments complex-wavelet --self-check --registration /path/to/non-ready-prospective.json --runs /path/to/stacking-controls --allowances /path/to/allowances.json --out /path/to/controls
python3 -m spikes.stacking_experiments complex-wavelet --reach-only --out .scratch/complex-wavelet/reach
python3 -m spikes.stacking_experiments complex-wavelet --resource-only --registration /path/to/source-ready.json --runs /path/to/stacking-controls --allowances /path/to/allowances.json --out /path/to/resources
python3 -m spikes.stacking_experiments complex-wavelet --split training --registration /path/to/quality-ready.json --runs /path/to/stacking-controls --allowances /path/to/allowances.json --out /path/to/training
python3 -m spikes.stacking_experiments complex-wavelet --repeat-only --registration /path/to/quality-ready.json --runs /path/to/stacking-controls --allowances /path/to/allowances.json --training-sha256 <archived-first-comparison-sha256> --out /path/to/repeat
```

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

## Alignment reference

`python3 -m spikes.stacking_experiments alignment-reference --self-check` validates fixed identifiers, arithmetic, and refusal paths without fitting frames. A stage records physical child arguments, inputs, streams, parsed output, and immutable artifacts. Source writes candidate snapshots and controls only; later independent evidence supplies ready envelopes. The parity collector records four current CLI runs before calibration; `--verify-parity` compares pinned CLI and calibration records without starting another fit. Controls require parity; quality, repeat, and real require allowance evidence. Repeat uses a pinned quality record and reuses its raw input bindings.
