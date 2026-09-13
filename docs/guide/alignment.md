# Aligning a stack

Handheld bracketing and focus breathing both shift the frame slightly from
shot to shot. `focusforge-cli stack` corrects for this before fusing —
alignment and the auto-crop that follows it are both on by default. The same
stage is reachable on its own through `focusforge-cli align`, which reports
each frame's recovered transform without fusing anything, useful for
checking a result before committing to a full stack run.

```bash
focusforge-cli align path/to/frames/
```

```
path/to/frames (8 frames)
pyramidal-ecc - Pyramidal ECC (reference=3 policy=Anchor failure_policy=strict failures=0)
rho = ECC correlation in [-1, 1], not a pixel distance
00.tif    tx=+12.41 ty=-3.07 scale=1.000842 rot=+0.0131deg  converged iters=12 rho=0.9814
...
03.tif    tx=+0.00 ty=+0.00 scale=1.000000 rot=+0.0000deg  converged iters=0 rho=1.0000 (reference)
...
auto-crop: 18,22 476x468
```

Every other frame registers against one reference frame (the middle frame of
the stack by default), reporting its recovered translation, scale and
rotation, an iteration count, and `rho` — the ECC correlation the solver
optimized, in `[-1, 1]`, not a pixel distance. The last line is the
auto-crop rectangle `stack` would apply: the largest axis-aligned rectangle
that every warped frame actually covers.

On the 61-frame reference macro stack this project is developed against,
alignment recovers up to 159 px of drift in translation-only mode, and up to
199 px when frames also breathe in scale and rotation.

## Choosing a mode

The aligner is `pyramidal-ecc`, an Enhanced Correlation Coefficient solver
run over an image pyramid. It has two modes, set with `--set mode=...`:

- **`translation`** — recovers a horizontal and vertical shift only. Cheaper,
  and enough for a tripod-mounted stack that only drifts.
- **`rotation-scale`** (the default) — also recovers scale and rotation, for
  a stack where the lens breathes as focus changes.

```bash
focusforge-cli align path/to/frames/ --set mode=translation
```

Run `focusforge-cli algorithms pyramidal-ecc` for the full parameter list —
iteration cap, convergence epsilon, pyramid depth, which frame is the
reference, and whether every frame registers against one anchor or chains
hop-by-hop through its neighbours.

## Failure is strict by default

A frame the solver cannot register fails the whole run: `stack` and `align`
both exit non-zero and name every frame that failed, rather than silently
falling back to an unregistered frame. `focusforge-cli align --help` and
`focusforge-cli stack --help` list the option to soften that policy.

## Checking a result visually

```bash
focusforge-cli align path/to/frames/ --write-warped .scratch/warped/
```

Writes one warped TIFF per input frame into the given directory, at the
frame's own source dimensions and uncropped, so the region the auto-crop
would cut is still visible for inspection.

## Skipping it

```bash
focusforge-cli stack path/to/frames/ -o out.tif --algo laplacian --no-align
```

`--no-align` fuses the raw frames unwarped; `--no-crop` keeps alignment on
but leaves the output at the full frame extent instead of cropping to the
common region. See [`reference/cli.md`](../reference/cli.md#stack) for every
`stack` and `align` flag.
