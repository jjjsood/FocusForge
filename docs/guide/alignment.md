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

Every other frame registers toward one reference frame (the middle frame of
the stack by default), by default hop-by-hop through its neighbours (see
`reference-policy` below). Each row reports its recovered translation, scale and
rotation, an iteration count, and `rho` — the ECC correlation the solver
optimized, in `[-1, 1]`, not a pixel distance. The last line is the
auto-crop rectangle `stack` would apply: the largest axis-aligned rectangle
that every warped frame actually covers.

On a 61-frame macro stack, alignment recovers up to 159 px of drift in
translation-only mode, and up to 199 px when frames also breathe in scale
and rotation.

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

`reference-policy` defaults to `chain`: each frame registers against its
neighbour on the way to the reference, which holds up when the first and last
frames differ a lot. Chain solves frames one after another; a 61-frame
44.8 MP stack aligns in about 40 s on the reference machine. `anchor`
registers every frame against the reference directly and in parallel, about
6 s on the same stack:

```bash
focusforge-cli align path/to/frames/ --set reference-policy=anchor
```

## Frame status

Each frame's row ends in the status its solve reached:

- `converged` — the step size fell below `epsilon-px`.
- `hit-cap` — `max-iterations` ran out while the steps were still shrinking.
- `stalled` — `max-iterations` ran out with the steps flat or oscillating; the
  best-correlation transform is kept. A middle pyramid level that loses
  correlation (typical of a defocused frame) also counts as `stalled`: it
  hands its best fit to the next finer level, which refines it.
- `diverged(<reason>)` — the frame did not register. `validation-failed`
  (JSON) is one reason: the fit converged, but the same transform leaves a
  large residual at most of the usable patches away from the fit region, so
  it is rejected rather than trusted. `weak-match` is another: the final
  correlation is below 0.2, so the best match found is a wrong optimum and is
  rejected. The other reasons are in
  [`reference/cli.md`](../reference/cli.md#align).

A pyramid level whose status is worse than the others sets the frame's
status.

## Accuracy

Before the final solve, the finest pyramid level of both the reference and the
frame is smoothed with three light low-pass passes, so depth-varying defocus
between two frames does not bias the fit. Coarser levels are not smoothed, and
a solve region that lies within the smoothing reach of the frame edge reads
the finest level unsmoothed.

Each frame's solve starts from a feature-based estimate when one is reliable.
That seed is accepted when its matches agree over enough of the frame: spread
across at least 6 cells of a 4x4 grid, or, when clustered in as few as 2
cells, when at least 75% of the matches support it. Otherwise the solve starts
from the identity.

## Registration failures use identity fallback

A frame that ends `diverged` keeps an identity transform and the run continues,
with the frame marked as a fallback in the report. Pass `--strict` to make
`stack` or `align` exit 1 and name every failed frame. `--no-align` skips the
stage when the frames are already registered.

If the aligned frames share no usable overlap, `stack` stops with an error
that says so; `--no-crop` keeps the full frame extent instead.

## Limits

Some inputs still defeat the solver:

- Heavy defocus and long series (on the order of 64 frames) can leave frames
  `diverged` (`correlation-decreased`). The full-resolution level gets four
  times the sample budget of the coarse levels. A fit whose final correlation
  stays below 0.2 is reported as `diverged` (`weak-match`), which catches the clear
  wrong fits, but `converged` still does not prove correctness on such
  stacks. Check them with `--write-warped` and look at the result, or compare
  each frame's transform with its neighbours'.
- Strongly periodic scenes (repeating patterns) can fail to align. Such
  frames normally end `diverged` rather than as a converged wrong fit.

Only `diverged` is a registration failure: `stalled`, `hit-cap` and
`converged` frames never fail a strict run. Use `--strict` when an
unregistered frame must stop the run; use `--no-align` when the frames are
already registered, or inspect the result with `--write-warped`.

## Reuse and hardware

`stack` stores each computed alignment in the stage cache, keyed on the
frames, the alignment parameters, the failure policy and the solver revision,
so a repeated run, or a re-fuse in the window with only fusion settings
changed, skips the solve. Frames are identified by their content, and an
alignment cached by an older solver revision is recomputed.
`--no-cache` forces a recompute.

Large pyramid levels run on the GPU when a usable adapter is present; the
rest of the solve is on the CPU. `--cpu` keeps everything on the CPU and
`--max-vram <SIZE>` lowers the device memory budget. If the device is lost
mid-run, the whole alignment is redone on the CPU, so every frame comes from
one backend. Both JSON reports name the backend in `align_backend`.

## Checking a result visually

```bash
focusforge-cli align path/to/frames/ --write-warped .scratch/warped/
```

Writes one warped TIFF per input frame into the given directory, at the
frame's own source dimensions and uncropped, so the region the auto-crop
would cut is still visible for inspection.

## Skipping it

```bash
focusforge-cli stack path/to/frames/ --algorithm laplacian --no-align
```

`--no-align` fuses the raw frames unwarped; `--no-crop` keeps alignment on
but leaves the output at the full frame extent instead of cropping to the
common region. In the window the same two choices are the **Enabled** switch
and **Auto-crop** (see [`guide/gui.md`](gui.md#settings)). See [`reference/cli.md`](../reference/cli.md#stack) for every
`stack` and `align` flag.
