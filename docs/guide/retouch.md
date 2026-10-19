# Retouching a result

The result image is the document. A retouch is an append-only log of soft
brush strokes, each blending one source frame over the image. Undo steps back
through the log, and a new stroke after an undo discards what redo held. The
Z-Map is analysis only: it feeds the rainbow overlay and the pipette and
renders nothing.

## From the command line

```bash
target/release/focusforge-cli retouch path/to/frames/frames.ffp --ops ops.json
```

appends every op in `ops.json` to the project as a new checkpoint, one per
op, each the previous one's child. They go on the head, or on another
checkpoint with `--from <id>`, which starts a branch. `--name <text>` names the
last one. The op list is a JSON array of brush strokes:

```json
[
  {"brush-stroke": {"source_id": 3, "path": [{"x": 10, "y": 10}, {"x": 40, "y": 12}], "radius": 8.0, "softness": 0.3}}
]
```

`source_id` is the 0-based frame index to blend in, `path` a list of `{x, y}`
points in image pixel coordinates, `radius` the stroke's half-width in pixels.
The stroke is fully opaque within `radius * (1 - softness)` and fades linearly
to nothing at `radius`; `softness` is in `[0, 1]`. Every op is checked before
anything is written, and an invalid one is refused by its 0-based position.

To see a result, export it:

```bash
target/release/focusforge-cli history list path/to/frames/frames.ffp
target/release/focusforge-cli export path/to/frames/frames.ffp --checkpoint <checkpoint-id> -o retouched.tif
```

See [`reference/cli.md`](../reference/cli.md#retouch) for every flag.

## Retouching in the window

A tab switches into editing once it has fused at least once, no fuse is
running, and its frame list still matches what was fused. Reorder, exclude or
add a frame afterward and the tab drops back to stacking until it is fused
again. The **STACKING / EDITOR** switch at the top of the dock makes the
change; once in **EDITOR**, the dock's `Images` panel turns into a legend, one
swatch and frame position per row instead of a name and capture time.

- **Brush** — drag across the **Composite** pane to blend the selected frame
  in. **Size** and **Soft** in the toolbar set the stroke's radius and edge
  softness; a bound wheel gesture adjusts size without opening the toolbar.
  Strokes blend on the GPU where one is available and on the CPU otherwise,
  with the same pixels either way.
- **Pipette** — click the pipette icon (or hold Alt) then click a pixel to
  make the frame the overlay shows there the active brush source, without
  painting. Alt+click alone picks without arming the pipette button.
- **Undo / Redo** — the toolbar's two icons, or their bound keys, step
  through the tab's own stroke history.
- **Overlay** — the bottom bar's toggle turns the rainbow legend on over the
  **Composite** pane, in stacking mode as well as in the editor; its slider
  sets opacity. It always shows the current state, painted strokes included:
  a pixel you painted from frame 3 shows frame 3's colour. It starts off and
  stays disabled until the tab has a fuse to show. If a project's Z-Map
  cannot be read, only the overlay and the pipette are unavailable; the image
  is unaffected.
- **Cursor mirroring** — the brush cursor and pipette crosshair, wherever the
  pointer sits in one pane, are echoed onto the other pane at the same
  underlying scene point, so the **Frame** and **Composite** panes stay in
  visual sync while editing.
- **Painting from either pane** — a stroke paints the same pixels whether the
  press lands in the **Frame** pane or the **Composite** pane, including when
  one of them is detached into its own window.

Saving strokes as a checkpoint, restoring an earlier one and branching are
covered in [checkpoints](history.md). **Export image** writes the result as
an image.
