# Spike: layered scenes and the signed halo label

Synthetic focus stacks in which a foreground layer with an alpha matte lies over a
background layer, both defocused per frame, and the label that says where a stacked
composite differs from the sharp truth beside the foreground's edge.

There is no `Cargo.toml`: the directory is covered by the workspace's `spikes`
exclusion, as for `spikes/synthetic-ground-truth/`. The module imports
`spikes/guided-filter-fusion/gff_fusion.py` (and through it `synth_stack.py`) for its blur,
texture, TIFF and edge-distance helpers and edits neither.

Requires Python 3.14 (`compression.zstd`) and numpy. `render` and `controls` need the
workspace built (`target/debug/focusforge-cli`).

## Running it

From the repository root:

```
python3 spikes/halo-detection/layered_scene.py --self-check
python3 spikes/halo-detection/layered_scene.py --self-check --break per-pixel-matte   # must exit 1
python3 spikes/halo-detection/layered_scene.py render --split all --out .scratch/halo-scenes
python3 spikes/halo-detection/layered_scene.py controls --out .scratch/halo-controls
```

`--self-check` needs only numpy. `--split` is `calibration`, `heldout` or `all`; the
scenes and seeds of each split are the `SPLITS` table. Every `stack` run passes `--cpu
--no-align --no-crop`, so no run needs the GPU and every output is 512 x 512.

## What `render` writes

Per scene and seed, `<out>/<scene>-s<seed>/`:

```
frames/NN.tif                 the layered frames, 16-bit RGB
truth.tif                     sharp foreground over sharp background
truth-depth.tif               frame index of the owning layer (uint16)
<method>.tif                  the stored composite of `focusforge-cli stack`
<method>-zmap.tif             its Z-Map, from `focusforge-cli export --zmap`
<method>-label.npy            signed float32 halo label
projects/<method>/            the stack run's project
```

for the methods `weighted-average`, `laplacian` and `depth-map`. The run ends with a
listing of every file and its width x height x samples. A missing CLI, a method the CLI
refuses or a depth outside `[0, frames - 1]` ends the run with a non-zero exit; no label is
written for a method without a composite.

The composite is the project's stored composite plane, read from the project file. For an
unedited project `export -o` writes the same samples; `controls` prints the largest
difference per method.

The label is a Gaussian low pass (`LABEL_SIGMA`) of `luma(composite) - luma(truth.tif)`:
positive where the composite is brighter than the truth (glow), negative where it is darker.

`controls` renders each calibration scene twice, layered and through `synth_stack.render()`
with the same truth image and depth, and prints the mean |label| beside the occluding edge
(bands of px from the alpha = 0.5 contour on the background side) against its level beyond
48 px, the glow share and the distance-to-edge ranking of halo pixels.

## Where the per-pixel model differs

`synth_stack.render()` blurs the whole truth image into a bank of Gaussian widths and picks,
for every output pixel, the bank entry of that pixel's own depth. A blurred bank entry
averages both layers beside a depth step, so a background pixel in a frame where it is
defocused still takes in its bright foreground neighbours, and the label of a stacked
composite shows that (`controls` prints the per-pixel levels).

What the per-pixel model lacks is the spread of a defocused foreground over the background.
A pixel's own depth selects one bank entry, so in the frame focused on the background the
background pixels come from the sharp entry. The signed mean luma minus truth 4-24 px outside
the matte edge there is +1.5, +0.2 and +1.4 counts on `bright-on-dark` seeds 1 to 3, against
+56.3, +80.4 and +62.0 for the layered render, which composites
`blur(alpha * F) + (1 - blur(alpha)) * background` per frame and so lets the blurred
foreground reach over the matte edge.

## Classical maps and the scorer

`classical_maps.py` builds two per-pixel maps from a stack method's Z-Map and composite:
map A (`risk-zones`), the bands beside depth steps, from the Z-Map alone; and map B, the
signed low-passed luminance inside those bands minus the far-band level on the same side
of the same step, either against the composite's own far band (`glow-self`) or against the
`depth-map` composite of the same stack (`glow-focus-map`, not written for `depth-map`).
A step is a pair of neighbouring Z-Map indices differing by at least `threshold`; unset
pixels (`65535`) are never a step and never in a band. A `median x median` filter over the
set Z-Map pixels runs first (`median = 1` is off). A band pixel's side is `+1` where its
nearest step pixel is the higher index, `-1` where lower. Map A scores `band_hi + 1 - distance`
inside `band_hi`; map B is NaN outside the band and where no far-band pixel of that side lies
in the `4 * band_hi + 1` window.

`halo_gate.py` scores map files with the gate: recall of halo pixels inside the top 15 % of
pixels for map A, AUC and sign agreement within 24 px of the contour for map B. A NaN score
counts as 0.

```
python3 spikes/halo-detection/classical_maps.py --self-check
python3 spikes/halo-detection/halo_gate.py --self-check
python3 spikes/halo-detection/classical_maps.py tune --scenes .scratch/halo-scenes
python3 spikes/halo-detection/classical_maps.py write --scenes .scratch/halo-scenes --out .scratch/halo-detection [--zmap-from depth-map]
python3 spikes/halo-detection/halo_gate.py score --scenes .scratch/halo-scenes --maps .scratch/halo-detection/risk-zones --unsigned
python3 spikes/halo-detection/halo_gate.py score --scenes .scratch/halo-scenes --maps .scratch/halo-detection/glow-self
python3 spikes/halo-detection/classical_maps.py map --arm glow-focus-map --composite C.tif --zmap Z.tif --reference R.tif --out M.npy
```

`tune` reads the calibration scenes only; `PARAMS` in `classical_maps.py` holds the sets it
chose, and `write` and `map` use them. `write` stores `float32` 512 x 512 maps as
`<out>/<arm>/<scene>-s<seed>/<method>-map.npy` for the held-out scenes; `--zmap-from depth-map`
adds `risk-zones-dm-zmap` and `glow-self-dm-zmap`, the same maps read from the `depth-map`
Z-Map. `score` prints, per method, each measure with its limit and `pass` or `fail`, and
`n/a` for a method the arm has no maps for; a map for only some scenes, or of a size other
than the label's, exits non-zero.

## Pretrained defocus detector

`pretrained_dbd.py` runs the Depth Distillation defocus-blur detector (MIT, arXiv 2007.08113)
unchanged on the rendered scenes' composites and writes its output as two `halo_gate.py` arm
directories per input size: map A (`dbd-risk-<size>`, the distilled depth output banded beside
its depth steps with `classical_maps.risk_map`) and map B (`dbd-blur-<size>`, the blur
probability). Both are unsigned, so `score` takes `--unsigned` for them; map B's AUC without the
flag is a diagnostic and its sign agreement does not apply. `<size>` is `320` (the detector's own
320 x 320 preprocessing) or `native` (the composite's size, which must be a multiple of 32 on both
sides).

Nothing it needs is tracked: the environment, the upstream code at its pinned commit, the weights
and the raw outputs live under `.scratch/halo-detection/` (`venv/`, `upstream/`, `weights/`,
`dbd-raw/`).

### The environment

Python 3.14, PyTorch 2.11.0 and torchvision 0.26.0 from the CUDA 12.8 wheel index (the card is a
Blackwell part, `sm_120`, which needs a CUDA 12.8 or newer build):

```
uv venv --python 3.14 .scratch/halo-detection/venv
uv pip install --python .scratch/halo-detection/venv/bin/python \
    --index-url https://download.pytorch.org/whl/cu128 torch==2.11.0 torchvision==0.26.0 numpy
```

`uv pip freeze` of the result is the pinned set (numpy, Pillow and the `nvidia-*` wheels come from
the same index):

```
cuda-bindings==12.9.7
cuda-pathfinder==1.6.0
cuda-toolkit==12.8.1
filelock==4.2.4
fsspec==2026.9.0
jinja2==3.1.6
markupsafe==3.0.3
mpmath==1.3.0
networkx==3.7
numpy==2.5.4
nvidia-cublas-cu12==12.8.4.1
nvidia-cuda-cupti-cu12==12.8.90
nvidia-cuda-nvrtc-cu12==12.8.93
nvidia-cuda-runtime-cu12==12.8.90
nvidia-cudnn-cu12==9.19.0.56
nvidia-cufft-cu12==11.3.3.83
nvidia-cufile-cu12==1.13.1.3
nvidia-curand-cu12==10.3.9.90
nvidia-cusolver-cu12==11.7.3.90
nvidia-cusparse-cu12==12.5.8.93
nvidia-cusparselt-cu12==0.7.1
nvidia-nccl-cu12==2.28.9
nvidia-nvjitlink-cu12==12.8.93
nvidia-nvshmem-cu12==3.4.5
nvidia-nvtx-cu12==12.8.90
pillow==13.0.0
setuptools==78.1.0
sympy==1.14.0
torch==2.11.0+cu128
torchvision==0.26.0+cu128
triton==3.6.0
typing-extensions==4.16.0
```

Saved to a file, the list installs the identical set: `uv pip install --python
.scratch/halo-detection/venv/bin/python --index-url https://download.pytorch.org/whl/cu128 -r <file>`.

`check-env` proves the build runs on the card: it prints the torch and CUDA versions and the device
name, and exits non-zero naming the cause when CUDA is unavailable, when the build has no kernels
for the card's `sm_` architecture, or when an integer-valued matrix product differs between the
CPU and the card. Every subcommand that needs the card (`check-env`, `polarity`, `infer`, `bench`)
refuses to run without it; only `bench --device cpu` uses the CPU.

### Running it

From the repository root, with `PY=.scratch/halo-detection/venv/bin/python`:

```
python3 spikes/halo-detection/pretrained_dbd.py --self-check            # numpy only, no torch
$PY spikes/halo-detection/pretrained_dbd.py check-env
$PY spikes/halo-detection/pretrained_dbd.py fetch                       # upstream code, res_best.pth, resnext.zip
$PY spikes/halo-detection/pretrained_dbd.py polarity --scenes .scratch/halo-scenes
$PY spikes/halo-detection/pretrained_dbd.py infer --size 320 --scenes .scratch/halo-scenes
$PY spikes/halo-detection/pretrained_dbd.py infer --size native --scenes .scratch/halo-scenes
$PY spikes/halo-detection/pretrained_dbd.py tune --scenes .scratch/halo-scenes
$PY spikes/halo-detection/pretrained_dbd.py write --scenes .scratch/halo-scenes --out .scratch/halo-detection
$PY spikes/halo-detection/pretrained_dbd.py bench --scenes .scratch/halo-scenes [--device cpu]
python3 spikes/halo-detection/halo_gate.py score --scenes .scratch/halo-scenes --maps .scratch/halo-detection/dbd-risk-320 --unsigned
```

The commands that use the card run under the shared GPU lock. `fetch` reads the upstream code
(`vinthony/depth-distillation` at commit `6a59c51059d78af344cbd50192726cbe488a9daf`, refused
unless its `LICENSE` is MIT) and both weights files from Google Drive with plain `urllib`; a body
that is an HTML page, shorter than its `Content-Length`, or of another SHA-256 than the table
`SHA256` in the script is refused and leaves no file. `--weights-url` replaces the Drive address.
The distilled depth output is quantized to 8 bits as the upstream notebook stores it before map
A, so map A's step threshold is in depth levels. `infer --split` limits the scenes to one split;
`tune` reads the calibration split only, per size, and its pick is frozen as `PARAMS` before
`write` runs (`write` refuses without it).

## Trained halo network

`halo_net.py` trains one small U-Net with two outputs on layered scenes and writes both maps as
`halo_gate.py` arm directories: `net-risk` (map A, a risk score in [0, 1], scored with `--unsigned`)
and `net-glow` (map B, the signed halo in counts, positive glow, scored with its AUC and sign
agreement). The weights, generated scenes and arm directories live under `.scratch/` and are not
tracked.

Input per pixel: the composite's luma over 65535, the Z-Map index over `frames - 1` (0 where the
index is unset) and a mask of set pixels. Targets, both explicit: map A is `(25 - d) / 25` for a pixel at
distance `d` (0 to 24 px) from the truth contour on either side, 0 beyond; map B is the signed label
(`luma(composite) - luma(truth)`, low-passed) over 100, so a positive output is glow and a negative
one darkening. The network has three poolings, widths 16-32-64-128, two 3x3 convolutions with
BatchNorm per level and a 1x1 head per map; BatchNorm in evaluation mode acts per pixel, so tiled
inference equals whole-image inference away from the border. Inference runs in 256 px tiles with a 64 px
context margin, reflect-padded, so any image size works.

Training scenes are generated at seeds 4-9 only, from the two procedural families (`bright-on-dark`,
`dark-on-bright`) and from eight 512 x 512 windows of `Mantis_Stacked.tif` (`WINDOWS`; found through
`FOCUSFORGE_REFERENCE_STACK`, read by tile) in the families `stack<k>-on-dark` and `dark-on-stack<k>`.
The `mantis` texture and every window within 512 px of the fixture's crop (3840, 2476, 512 x 512) are
refused, as is any held-out scene or seed outside 4-9; `generate` and `train` check this first.
Per scene the frame count, depths, blur step and noise are drawn from the seed. The calibration
scenes (seeds 1-3) pick the checkpoint, and `write` refuses unless `WEIGHTS_SHA256` in the script is
pinned and the weights file matches it.

From the repository root, with `PY=.scratch/halo-detection/venv/bin/python` (the environment above):

```
python3 spikes/halo-detection/halo_net.py --self-check                   # numpy only, no torch
python3 spikes/halo-detection/halo_net.py generate --list-windows
python3 spikes/halo-detection/halo_net.py generate --seeds 4-9 --out .scratch/halo-train
$PY spikes/halo-detection/halo_net.py train --seeds 4-9 --scenes .scratch/halo-train --calibration .scratch/halo-scenes
$PY spikes/halo-detection/halo_net.py tile-check --scenes .scratch/halo-scenes
$PY spikes/halo-detection/halo_net.py write --scenes .scratch/halo-scenes --out .scratch/halo-detection
python3 spikes/halo-detection/halo_gate.py score --scenes .scratch/halo-scenes --maps .scratch/halo-detection/net-risk --unsigned
python3 spikes/halo-detection/halo_gate.py score --scenes .scratch/halo-scenes --maps .scratch/halo-detection/net-glow
$PY spikes/halo-detection/halo_net.py bench --scenes .scratch/halo-scenes [--device cpu]
$PY spikes/halo-detection/halo_net.py infer --composite C.tif --zmap Z.tif --frames N --out-dir D
```

`train` uses the card (6000 steps of 16 random 256 px crops with the eight flips and quarter turns,
Adam 1e-3 with cosine decay, seed 0; loss is binary cross-entropy on map A plus a smooth-L1 on map B
weighted 4 times within 24 px of the contour) and stops past 30 minutes or 4 GiB of graphics memory.
Every 500 steps the checkpoint is scored on the calibration scenes at the gate's measures and the one
with the most measure-and-method cells at their limit (then the larger summed margin) is kept as
`.scratch/halo-detection/net/halo-net.pt`. `infer` writes `risk.npy` and `glow.npy` for one composite
and Z-Map of any size. Commands that use the card run under the shared GPU lock; `generate` and
`--self-check` need no card.
