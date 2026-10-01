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
python3 spikes/guided-filter-fusion/gff_fusion.py fuse DIR -o OUT.tif [--scale S] [--tile T --margin M]
python3 spikes/guided-filter-fusion/gff_fusion.py score CASE_DIR OUT.tif
python3 spikes/guided-filter-fusion/gff_fusion.py reach
python3 spikes/guided-filter-fusion/gff_fusion.py scale --out .scratch/gff-scale
python3 spikes/guided-filter-fusion/gff_fusion.py montage OUT.png --rect Y0,Y1,X0,X1 A.tif B.tif
```

- `--self-check` builds a 48x64, N = 3 stack and requires every detector (blur, invalid-source pull,
  channel disagreement) to stay silent on the honest prototype and fire on its broken variant
  (`--break uniform-weights`, `ignore-mask`, `per-channel-guide`). It also fuses identical constant and
  edge stacks, which must come back unchanged. Exit 0 means every run behaved.
- `cases` builds registered synthetic stacks with `spikes/synthetic-ground-truth/synth_stack.py --frames-only`
  (cases `halo`, `flat`, `easy`, `hairs`, `tint`, `border`, and `align-border`, whose invalid-border mask comes from
  `focusforge-cli align --json` transforms), runs each baseline as `stack ... --no-align --no-crop --no-gpu` then
  `export`, scores every composite and prints per-metric spread over the seeds. `--tile-size` is the size of the
  read rectangle, whose interior is the tile minus twice the method's margin, so a one-tile whole-image control needs
  an interior of at least the image size (`512` gives four tiles at 512x512; `640` suffices at 512x512 with a margin-12 method; larger is safe, the default is `1024`).
  `--candidate` adds the prototype. `mantis` runs `fixtures/mantis-512` and `stack-crop` a crop of the large reference
  stack (`--rect X,Y,W,H`, `--crop-frames`); neither has a truth image, so they write a side-by-side `montage.png`
  instead of metrics.
- `fuse --tile T --margin M` assembles the result tile by tile with `M` px of context; compare it with the
  whole-image output using `tools/tiff-diff.py`.
- `reach` puts an impulse in one frame of a constant two-frame stack and prints the farthest pixel whose weight
  moves, per threshold.
- `scale` renders N-frame synthetic stacks, runs the prototype in a fresh process and the baselines under
  `/usr/bin/time -v`, and records wall time, peak RSS and frame reads.

Every subcommand prints JSON with the command line it ran.

## Metrics

All against the case's `reference.tif`, over pixels with at least one valid source: `psnr` and block `ssim` as in
`synth_stack.py`; `halo_rms` (+/-8 px band around the depth step), `flat_std` (flat patch), `hair_ret` (line contrast
retained against the reference), `chroma_err` (mean `|channel/luma - reference channel/luma|` over unclipped pixels),
`border_pull` (shortfall below the darkest valid source at pixels that also have an invalid source).

## Dependencies and data

numpy only (TIFF input and output go through `tools/lib/rawtiff.py`). The large reference stack is read only
through the `FOCUSFORGE_REFERENCE_STACK` directory and only as rectangle crops; with the variable unset, `stack-crop`
stops with a message naming it.
