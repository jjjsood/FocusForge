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

## Frame status

Each frame's row ends in the status its solve reached:

- `converged` — the step size fell below `epsilon-px`.
- `hit-cap` — `max-iterations` ran out while the steps were still shrinking.
- `stalled` — `max-iterations` ran out with the steps flat or oscillating; the
  best-correlation transform is kept.
- `diverged(<reason>)` — the frame did not register. `validation-failed`
  (JSON) is one reason: the fit converged, but the same transform leaves a
  large residual at most of the usable patches away from the fit region, so
  it is rejected rather than trusted. The other reasons are in
  [`reference/cli.md`](../reference/cli.md#align).

A pyramid level whose status is worse than the others sets the frame's
status.

## Failure is strict by default

A frame that ends `diverged` fails the whole run: `stack` and `align` both
exit 1 and name every frame that failed, rather than silently falling back to
an unregistered frame. `--lax` on either command softens each failed frame to
an identity transform, exits 0 and reports the failure count on stderr.

If the aligned frames share no usable overlap, `stack` stops with an error
that says so; `--no-crop` keeps the full frame extent instead.

## Limits

Some inputs still defeat the solver:

- Heavy defocus and long series (on the order of 64 frames) can leave frames
  `diverged` (`correlation-decreased`). On the same stacks a frame can also
  end `converged` with a wrong transform and no warning, off by anything from
  a pixel to over a hundred, so `converged` does not prove correctness there.
  Check such a stack with `--write-warped` and look at the result, or compare
  each frame's transform with its neighbours'.
- Strongly periodic scenes (repeating patterns) can register to the wrong
  period.

Only `diverged` is a failure: `stalled`, `hit-cap` and `converged` frames
never fail a strict run. For a stack with `diverged` frames use `--lax` to
fuse anyway, or `--no-align` when the frames are already registered.

## Reuse and hardware

`stack` stores each computed alignment in the stage cache, keyed on the
frames, the alignment parameters, the failure policy and the solver revision,
so a repeated run, or a re-fuse in the window with only fusion settings
changed, skips the solve. `--no-cache` forces a recompute.

Large pyramid levels run on the GPU when a usable adapter is present; the
rest of the solve is on the CPU. `--no-gpu` keeps everything on the CPU and
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
focusforge-cli stack path/to/frames/ --algo laplacian --no-align
```

`--no-align` fuses the raw frames unwarped; `--no-crop` keeps alignment on
but leaves the output at the full frame extent instead of cropping to the
common region. In the window the same two choices are the **Enabled** switch
and **Auto-crop** (see [`guide/gui.md`](gui.md#aligning-in-the-window)). See [`reference/cli.md`](../reference/cli.md#stack) for every
`stack` and `align` flag.
