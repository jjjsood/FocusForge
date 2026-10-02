# Guided-filter fusion prototype

An offline CPU prototype of two-scale guided-filter fusion (Li, Kang, Hu, 2013) for
registered focus stacks, plus the measurements that compare it with the three
fusion methods `focusforge-cli` already ships (`weighted-average`, `laplacian`,
`depth-map`). It is a numpy reference, not a product path: nothing under `crates/`
uses it and no default changes.

## What it computes

On `u16 / 65535` intensities, per frame:

- base layer = 31x31 box mean, detail layer = frame minus base (per channel);
- saliency = |3x3 Laplacian| of the luma (channel mean), smoothed by an 11x11 Gaussian (sigma 5);
- pass 1 keeps a running argmax of saliency over all frames (binary winner map);
- pass 2 filters each frame's winner map with a guided filter driven by that frame's luma:
  base weights `r = 45, eps = 0.3`, detail weights `r = 7, eps = 1e-6`, clipped to `[0, 1]`;
- result = sum(Wb * base) / sum(Wb) + sum(Wd * detail) / sum(Wd).

Frames are read twice (once per pass) and only accumulators survive between frames, so
working state does not grow with the frame count. All radii scale with `--scale`
(1.0 is set for a 0.25 MP image). Invalid pixels (a mask per frame, `masks.npy` beside the
frames) get zero winner and zero weight, and every window statistic counts valid pixels
only; a pixel with no valid source is reported in `uncovered`, never filled.

## Running it

From the repository root, with the workspace built (`tools/with-lock.sh build -- cargo build --workspace`)
and numpy installed:

```
python3 spikes/guided-filter-fusion/gff_fusion.py --self-check
python3 spikes/guided-filter-fusion/gff_fusion.py cases --out .scratch/gff --candidate
python3 spikes/guided-filter-fusion/gff_fusion.py cases --out .scratch/gff-eq --cases gain-ramp,gain-offset,flicker --equalize both --candidate
python3 spikes/guided-filter-fusion/gff_fusion.py fuse DIR -o OUT.tif [--scale S] [--tile T --margin M]
python3 spikes/guided-filter-fusion/gff_fusion.py score CASE_DIR OUT.tif
python3 spikes/guided-filter-fusion/gff_fusion.py reach
python3 spikes/guided-filter-fusion/gff_fusion.py scale --out .scratch/gff-scale
python3 spikes/guided-filter-fusion/gff_fusion.py montage OUT.png --rect Y0,Y1,X0,X1 A.tif B.tif
```

- `--self-check` builds a 48x64, N = 3 stack and requires every detector (blur, invalid-source pull,
  channel disagreement, glint loss) to stay silent on the honest prototype and fire on its broken variant
  (`--break uniform-weights`, `ignore-mask`, `per-channel-guide`; the glint detectors run on a stack with two saturated discs).
  It also checks the baseline command line (registered input, `--set` forwarding, the coverage arm's alignment flags),
  the coverage masks derived from transforms and that every case's crop rectangle lies inside the frame. It also fuses identical constant and
  edge stacks, which must come back unchanged. Exit 0 means every run behaved.
- `cases` builds registered synthetic stacks with `spikes/synthetic-ground-truth/synth_stack.py --frames-only`
  (cases `halo`, `flat`, `easy`, `hairs`, `glints`, `tint`, `border`, the brightness-drift cases `gain-ramp`, `gain-offset`, `flicker`, and
  `align-border`, whose invalid-border mask comes from `focusforge-cli align --json` transforms), runs each baseline as
  `stack ... --no-align --no-crop --no-gpu` then `export`, scores every composite and prints per-metric spread over the
  seeds. `--tile-size` is the size of the read rectangle; the tile margins (160/80/40 px for Laplacian, 12 and 16 px for
  Weighted Average and Depth Map) make a tiled run equal to a one-tile run, so it changes no method's output. `1024` at
  512x512 (interior 704) and `2048` at a 1024x1024 crop stay one-tile controls.
  `--candidate` adds the prototype. `mantis` runs `fixtures/mantis-512` and `stack-crop` a crop of the large reference
  stack (`--rect X,Y,W,H`, `--crop-frames`); neither has a truth image, so they write a side-by-side `montage.png`
  instead of metrics. A synthetic case writes a `montage.png` too: the truth first, then each method, at full resolution
  over the case's class rectangle (`CROPS`: the `halo` band, the flat patch, the hair region, two glints, a border corner).
- `--stack-set KEY=VAL` (repeatable) is forwarded to every baseline as `stack --set KEY=VAL`, which runs a method at a
  neighbouring parameter value; a method that does not declare the key makes the run stop with the CLI's error, so pass
  `--methods` with it. The row records the pairs under `stack_set`.
- `--product-coverage` turns `align-border` into an end-to-end coverage arm: the baselines stack `raw/` with the product's
  own alignment (`--align-algo pyramidal-ecc --align-set mode=rotation-scale --no-crop`) instead of `--no-align`, and the
  row's masks come from that run's own `align.alignments[].transform.m`, scored against that run's own frames as fused
  (`export --frame`), so frames and masks share one set of transforms. The row carries `coverage: "product"`, the run's
  `align_reference` and `align_vs_align_command`, the largest difference of its transforms from those of the `align`
  command on the same frames (the two need not agree); `psnr` and `ssim` then depend on the alignment and on which frame it chose as reference, and
  `border_pull` does not. Without the flag the case runs `--no-align` on the warped frames with the rig's own masks, which
  the product never sees, so its baselines pull at the borders.
- `--equalize off|on|both|product` (default `off`) adds a per-frame brightness equalization before fusion. `on` writes
  `equalized/` beside each case (frames, `masks.npy` copy and `equalize.json`) and every method reads it; `both` runs the
  two arms on identical frames, tags each row with its `arm`, adds `vs_off` (max absolute difference, differing samples,
  mean absolute difference over the OFF mean) to each ON row and prints an ON-minus-OFF table. The default leaves cases,
  commands and outputs unchanged. `product` runs the product's own equalization (`stack --set equalize=true`) on the
  unequalized frames, tags its rows `product` and stores the report's `equalization` block beside each row; the rig's
  equalizer is not involved.
- `fuse --tile T --margin M` assembles the result tile by tile with `M` px of context; compare it with the
  whole-image output using `tools/tiff-diff.py`.
- `reach` puts an impulse in one frame of a constant two-frame stack and prints the farthest pixel whose weight
  moves, per threshold.
- `scale` renders N-frame synthetic stacks, runs the prototype in a fresh process and the baselines under
  `/usr/bin/time -v`, and records wall time, peak RSS and frame reads.

## Equalization

`equalize(dir, out_dir)` gives every frame one gain shared by all channels: the median of reference luma over frame
luma (channel mean of the encoded values) on pixels that are valid in both frames (from `masks.npy`, never from
`sample == 0`) and whose reference luma lies in 5 % to 95 % of 65535. The reference is frame `n // 2` (gain 1). A frame
with fewer than 1000 usable pixels gets gain 1 and is flagged `thin`. The applied gain is clipped to `[0.8, 1.25]`,
multiplies the encoded values and saturates at 65535 without wrap; invalid pixels stay zero. `equalize.json` records the
reference, band, bound and, per frame, the estimated and applied gain, the clamp flag, `samples_used` and
`samples_saturated`. `--self-check` also recovers known gains on a tiny stack and requires each deliberately broken
estimator (`wrong-reference`, `ignore-mask`, `unbounded-gain`) to be caught.

The drift cases are the `halo` frames with a per-frame gain and offset applied after noise (frame 4 unchanged):
`gain-ramp` (gain 0.92 to 1.06), `gain-offset` (the same ramp plus -300 to +225 counts) and `flicker` (alternating
1 +/- 0.06 and one 0.72 dip whose correction exceeds the bound). A gain cannot remove an additive offset.

Every subcommand prints JSON with the command line it ran.

## Metrics

All against the case's `reference.tif`, over pixels with at least one valid source: `psnr` and block `ssim` as in
`synth_stack.py`; `halo_rms` (+/-8 px band around the depth step), `flat_std` (flat patch), `hair_ret` (line contrast
retained against the reference), `glint_core` (mean luma over the glint discs divided by the truth's, 1 keeps them) and
`glint_ring` (luma RMS against the truth in the annuli 2 to 8 px outside the discs), `chroma_err` (mean `|channel/luma - reference channel/luma|` over unclipped pixels),
`border_pull` (shortfall below the darkest valid source at pixels that also have an invalid source), `psnr_lvl` (`psnr`
after the one global gain that best fits the composite to the reference, so a level offset does not count).

## Dependencies and data

numpy only (TIFF input and output go through `tools/lib/rawtiff.py`). The large reference stack is read only
through the `FOCUSFORGE_REFERENCE_STACK` directory and only as rectangle crops; with the variable unset, `stack-crop`
stops with a message naming it.
