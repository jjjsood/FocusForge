# Alignment

No two frames of a focus [stack](../glossary.md#stack) line up perfectly. The camera drifts a little
on the tripod or between handheld shots, and most lenses change the image
size slightly as focus moves, an effect called
[focus breathing](../glossary.md#focus-breathing). Fused as they are, such
frames give doubled edges and smeared detail.
[Alignment](../glossary.md#alignment) fixes this before fusing: FocusForge
moves every frame so that it lines up with one
[reference frame](../glossary.md#reference-frame). It is on by default and
needs no attention for most stacks. This page explains the **ALIGN** section
of the dock's **Settings** tab (see [Stacking settings](stacking.md) for the
tab as a whole) and what to do when a frame cannot be aligned.

## What alignment does

When you click **Fuse stack**, alignment runs after the brightness of the
frames has been evened out and before they are fused. The bottom bar reads
**Aligning** with the number of frames done (such as "3 of 8").

For every frame, FocusForge:

1. **Picks the frame to compare with.** By default every frame is compared
   with its neighbour, step by step, on the way to the middle frame of the
   stack (see [Chain or Anchor](#chain-or-anchor)).
2. **Finds the movement.** It compares the two frames coarsely first, then
   improves the result at larger and larger sizes up to full size. It looks
   for a shift, and with the default **Rotation + Scale** mode also a change
   of size and a small rotation.
3. **Checks the answer.** A result that does not hold up across the frame, or
   whose match is too weak to be trusted, is rejected, and the frame counts as
   not aligned (see
   [When a frame cannot be aligned](#when-a-frame-cannot-be-aligned)).
4. **Moves the frame** into place for fusing. Your source files are not
   changed.

Finally, **[Auto-crop](../glossary.md#auto-crop)** in the **FINISH** section trims the result to the area
every aligned frame covers.

## The ALIGN settings

![The ALIGN section with More... open: the Enabled switch on, Algorithm Pyramidal ECC, Mode Rotation + Scale, Max Iterations 400, Epsilon (px) 0.0001 on a slider with the scale 1e-9, 1e-6, 1e-3 and 1, Pyramid Depth 3, Reference Policy Chain and Reference Frame Middle.](../assets/manual/alignment/align-more.png)

*The ALIGN section with its More... bar open, at the defaults.*

**ALIGN** shows the **Enabled** switch and the **Algorithm** menu; the rest
sits under its **More...** bar. **Pyramidal ECC** is the only alignment
method, so the menu has one entry.

| Setting | Range | Default | What it does |
|---|---|---|---|
| **Enabled** | on / off | on | Lines every frame up with the reference frame before fusing. Off fuses the frames exactly as they were read. |
| **Algorithm** | **Pyramidal ECC** | **Pyramidal ECC** | The alignment method. |
| **Mode** | **Translation only**, **Rotation + Scale** | **Rotation + Scale** | Which movements are corrected: a shift only, or a shift plus a change of size and a small rotation. |
| **Max Iterations** | 1 to 1000 | 400 | The most adjustment steps alignment may take for a frame at each size. |
| **Epsilon (px)** | 0.000000001 to 1 | 0.0001 | How small, in pixels, an adjustment step must be before alignment counts a frame as settled. |
| **Pyramid Depth** | 0 to 16 | worked out from the frame size | How many smaller, coarser versions of the frame are compared before the full-size one. |
| **Reference Policy** | **Anchor**, **Chain** | **Chain** | Whether every frame is compared directly with the reference frame, or step by step through its neighbours. |
| **Reference Frame** | **First**, **Middle**, **Last** | **Middle** | Which frame the others are lined up with. |

## Choose a mode

| Mode | Use it for |
|---|---|
| **Rotation + Scale** (default) | Stacks focused with the lens's focus ring or by the camera's own focus bracketing, where the image grows or shrinks a little from frame to frame; handheld stacks, which also twist slightly. When in doubt, use this. |
| **Translation only** | A stack shot on a focus rail with a lens that does not breathe, where the only movement is a small drift. It solves for less, so it is cheaper. Also worth a try when **Rotation + Scale** reports frames it could not align. |

To change it: open **More...** under **ALIGN**, click the **Mode** field and
choose an entry.

## Chain or Anchor

**Reference Policy** decides what each frame is compared with:

- **Chain** (default) compares each frame with its neighbour, step by step
  towards the reference frame. Neighbouring frames look alike, so this holds
  up even when the first and last frames of a long stack look very different
  (one sharp at the front, the other at the back).
- **Anchor** compares every frame directly with the reference frame, all at
  the same time. It is faster than **Chain** on a machine with many processor
  cores. Use it for short stacks or stacks whose frames all look similar,
  when speed matters.

## Choose the reference frame

**Reference Frame** picks the frame that stays where it is; every other frame
is moved onto it. The result therefore has the framing and image size of the
reference frame, and an exported image carries the camera data (EXIF) of
that frame.

- **Middle** (default) keeps the distance to the farthest frame as small as
  possible, which suits most stacks.
- **First** or **Last** when one end of the stack has the framing you want,
  for example when the image size changes a lot across a long focus-ring stack
  and you want the result at the size of the nearest or farthest focus.

## Settings you rarely change

**Max Iterations**, **Epsilon (px)** and **Pyramid Depth** control how hard
alignment works. Their defaults suit everything from small frames to very
large ones; leave them unless you have a reason.

- **Max Iterations** is the most adjustment steps a frame gets at each size.
  Frames that settle earlier stop early. A frame that reaches the limit is not
  rejected: it keeps the best position found so far.
- **Epsilon (px)** is the precision at full size, in pixels. The default, a
  ten-thousandth of a pixel, is already far finer than you can see; a larger
  value makes alignment a little faster and less exact. The slider spans
  several powers of ten, marked `1e-9` to `1`.
- **Pyramid Depth** is how many coarser versions of the frame are compared
  before the full-size one. The default is worked out from your frames. A
  deeper setting can catch larger movements but has less detail to work with
  at its coarsest step.

## Switch alignment off

![The Settings tab with the ALIGN Enabled switch off: the Algorithm field and the More... bar below it are greyed, the ALIGN heading has no DEFAULT badge, and Reset All to Default at the bottom is available.](../assets/manual/alignment/align-off.png)

*Alignment switched off: the rest of the ALIGN section is greyed.*

Click the **Enabled** switch under **ALIGN** to turn alignment off. The rest
of the section greys out and keeps its values. The next fuse then uses the
frames exactly as they were read, and the result keeps the full frame.

Switch it off when:

- the frames are already lined up, for example because you aligned them in
  another program and exported them;
- the camera and subject were truly fixed and the lens does not breathe, and
  you want to save the time alignment takes;
- alignment makes things worse, for example on a subject made of a repeating
  pattern, and you want to compare.

Switching alignment off (or off and on again) removes the **ALIGN**
section's **DEFAULT** badge; **Reset All to Default** brings it back (see
[Reset All to Default](stacking.md#reset-all-to-default)).

## Fusing again is quicker

Aligning is often the slowest part of a fuse, so FocusForge keeps each
alignment it computes. When you fuse the same frames again after changing
only the [fusion method](../glossary.md#fusion-method) or its settings under
**FUSE**, the bottom bar briefly reads **Aligning** with "from cache" instead
of a frame count, and the fuse is much quicker. The kept alignment is found
again even when the files were moved or renamed; frames you reshoot are
aligned afresh. To delete it and free the disk space, see
[Free disk space used by earlier runs](settings-window.md#free-disk-space-used-by-earlier-runs).

## When a frame cannot be aligned

![The dialog "Keep this fused result?" with the text "00.tif and 01.tif could not be aligned and were fused as they are." and the buttons Discard and Keep.](../assets/manual/alignment/keep-dialog.png)

*Two frames of the sample stack could not be aligned.*

A frame that cannot be aligned does not stop the fuse. FocusForge uses it
unmoved, exactly where it was in the camera, finishes the run and then asks **Keep this fused result?**, naming
the frames it could not align (past five, the rest are counted). The dialog
needs an answer: **Escape** does not close it.

- **Keep** keeps the result. A notification "N frames unaligned" (or
  "1 frame unaligned") records it; see
  [Projects, notifications and quitting](batch-runs.md).
- **Discard** throws the result away. The project stays as it was before the
  fuse.

An unaligned frame only matters where it contributed to the result. Very
blurred frames, such as the first frames of a stack focused on the background,
often cannot be aligned because they hold little detail, but they also
contribute little; that is what happens in the sample stack above, and its
result is fine. Before you decide, look at the result for doubled edges or
smeared detail, and use the [Z-Map](../glossary.md#z-map) overlay to see where the named frames were
used (see
[See where each pixel came from](gui.md#see-where-each-pixel-came-from)).

If the result shows the problem:

1. Click **Discard**.
2. Try one change at a time and fuse again:
   - Leave the named frames out with **Exclude** in
     [the Images panel](images-panel.md), when they add nothing the
     neighbouring frames do not.
   - Switch **Mode** to **Translation only** when the stack was shot on a rail
     and does not breathe.
   - Switch **Reference Policy** between **Chain** and **Anchor**.
3. If the frames are already lined up, switch alignment off.

## What alignment cannot fix

Alignment moves each frame as a whole: it shifts, scales and rotates it. It
cannot fix:

- part of the subject moving relative to the rest (an antenna, a leaf in the
  wind);
- a change of perspective when the camera tilts or swings between frames;
- every frame of heavily defocused or very long series (on the order of 64
  frames): some may end up not aligned, and on such stacks even a frame that
  is reported as aligned is worth checking in the result;
- strongly repeating patterns, which can match in more than one position.
  Such frames are normally reported as not aligned rather than placed wrongly.

[Shooting a stack](../stacking/shooting-practice.md#what-alignment-absorbs)
explains how to shoot so that alignment has an easy job, and
[Common failure images](../stacking/failure-images.md#motion-between-frames)
shows what movement looks like in a result.

## From the command line

The same aligner runs in `focusforge-cli stack`, where alignment and the
auto-crop that follows it are both on by default, and on its own in
`focusforge-cli align`, which reports each frame's recovered movement without
fusing anything:

```bash
focusforge-cli align path/to/frames/
```

Each row reports the frame's recovered translation, scale and rotation, the
number of steps taken, and `rho`, how well the frame matches the reference, from
-1 to 1 (not a pixel distance). The last line is the auto-crop rectangle
`stack` would apply: the largest axis-aligned rectangle every warped frame
covers.

The **ALIGN** settings are the `pyramidal-ecc` parameters `mode`
(`translation`, `rotation-scale`), `max-iterations`, `epsilon-px`,
`pyramid-depth`, `reference-policy` (`anchor`, `chain`) and
`reference-frame` (`first`, `middle`, `last`). `align` takes them with
`--set`, `stack` with `--align-set`:

```bash
focusforge-cli align path/to/frames/ --set mode=translation
focusforge-cli align path/to/frames/ --set reference-policy=anchor
focusforge-cli stack path/to/frames/ --algorithm laplacian --align-set mode=translation
```

`focusforge-cli algorithms pyramidal-ecc` prints every parameter with its
range, default and description. See
[`reference/cli.md`](../reference/cli.md#align) for every `align` flag and
[`reference/cli.md`](../reference/cli.md#stack) for every `stack` flag.

### Frame status

Each frame's row ends in a status. `converged`, `hit-cap` and `stalled` mean
the frame lined up (the last two stopped before settling fully and keep the
best position found). `diverged(<reason>)` means it did not line up, for
example `weak-match` (the best match is too weak to trust) or
`validation-failed` (the movement found does not hold across the frame); the
other reasons are in [`reference/cli.md`](../reference/cli.md#align).

<a id="registration-failures-use-identity-fallback"></a>

### A frame that does not line up

A frame that ends `diverged` is used unmoved and the run continues, with the
frame marked in the report. Pass `--strict` to make `stack` or `align` exit 1
and name every failed frame. `--no-align` skips the stage when the frames are
already lined up. If the aligned frames share no usable overlap, `stack` stops
with an error that says so; `--no-crop` keeps the full frame instead.

### Using the graphics card and earlier results

`stack` keeps each computed alignment, so a repeated run skips it; `--no-cache`
recomputes it ([Batch runs from the command line](../reference/batch-runs.md)
describes where it is kept). The graphics card speeds up the largest steps
when a usable one is present; `--cpu` keeps everything on the processor. The
result is the same either way, and both JSON reports name the one used in
`align_backend`. `--max-vram <SIZE>` and `--max-host-memory <SIZE>` lower the
memory the alignment may use.

### Checking a result visually

```bash
focusforge-cli align path/to/frames/ --write-warped .scratch/warped/
```

Writes one warped TIFF per input frame into the given directory, at the
frame's own source dimensions and uncropped, so the region the auto-crop
would cut is still visible. Use it, or compare each frame's transform with its
neighbours', to check stacks where `converged` alone does not prove a correct
fit.

### Skipping it

```bash
focusforge-cli stack path/to/frames/ --algorithm laplacian --no-align
```

`--no-align` fuses the frames unwarped; `--no-crop` keeps alignment on but
leaves the output at the full frame extent instead of cropping to the common
region. In the window the same two choices are the **Enabled** switch under
**ALIGN** and **Auto-crop** under **FINISH** (see
[Stacking settings](stacking.md#finish-trim-the-edges)).
