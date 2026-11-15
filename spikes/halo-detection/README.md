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
