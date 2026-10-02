# Retouching a Z-Map

A stack's Z-Map names, for every output pixel, which source frame the
composite drew it from. Retouching never touches pixels: every edit
repaints entries in that map, so undo is exact and a re-render always
reflects the map as it stands now.

## The op list (`focusforge-cli retouch`)

```bash
target/release/focusforge-cli export path/to/frames/frames.ffp --zmap zmap.tif
```

writes the Z-Map out of a project as a standalone TIFF. An op list is a JSON
array of brush strokes:

```json
[
  {"brush-stroke": {"source_id": 3, "path": [{"x": 10, "y": 10}, {"x": 40, "y": 12}], "radius": 8.0, "softness": 0.3}}
]
```

`source_id` is the 0-based frame index to paint, `path` a list of `{x, y}`
points in Z-Map pixel coordinates, `radius` the stroke's half-width in
pixels, `softness` in `[0, 1]` how far the edge blends into the
surrounding indices rather than cutting sharply.

```bash
target/release/focusforge-cli retouch \
  --zmap zmap.tif --ops ops.json --out edited-zmap.tif
```

writes `edited-zmap.tif`: every op in `ops.json` applied in order. Add
`--undo N` to drop the last `N` ops before writing, and `--redo N` (never
more than `--undo`) to re-apply that many of them — the same unlimited
undo/redo history the window keeps, available headlessly.

To see the result rather than just the map, add `--frames` (the same frame
directory the stack came from) with `--render` and/or `--overlay`:

```bash
target/release/focusforge-cli retouch \
  --zmap zmap.tif --ops ops.json --out edited-zmap.tif \
  --frames path/to/frames/ --render recomposited.tif --overlay overlay.png
```

`--render` recomposites the edited map into a 16-bit TIFF, gathering each
pixel from the source frame its (possibly just-edited) index names.
`--overlay` blends a rainbow legend over that same recomposite and writes
it as an 8-bit PNG: one hue per source frame, spread evenly around the
wheel, with a fixed grey for any pixel no source ever claimed. `--opacity`
(`0.0`–`1.0`, default `0.65`) sets how strongly the legend colour shows
through; `--solo <INDEX>` desaturates every source except the one named,
so its footprint stands out against the rest.

See [`reference/cli.md`](../reference/cli.md#retouch) for every flag.

## Retouching in the window

A tab switches into editing once it has fused at least once, no fuse is
running, and its frame list still matches what was fused — reorder,
exclude or add a frame afterward and the tab drops back to stacking until
it is fused again. The **STACKING / EDITOR** switch at the top of the dock
makes the change; once in **EDITOR**, the dock's `Images` panel turns into
a legend, one swatch and frame position per row instead of a name and
capture time.

- **Brush** — drag across the right (composite) pane to paint the
  currently selected source's index. **Size** and **Soft** in the toolbar
  set the stroke's radius and edge softness; a bound wheel gesture adjusts
  size without opening the toolbar.
- **Pipette** — click the pipette icon (or hold Alt) then click a pixel to
  make its source the active brush source, without painting. Alt+click
  alone picks without arming the pipette button.
- **Undo / Redo** — the toolbar's two icons, or their bound keys, step
  through the tab's own op history. Undo goes back to the map as it stood
  before the last op; a fresh op after an undo discards whatever redo
  history followed it.
- **Overlay** — the bottom bar's toggle turns the rainbow legend on over
  the composite pane, in stacking mode as well as in the editor; its slider
  sets opacity, matching `retouch --overlay --opacity` exactly. It starts
  off and stays disabled until the tab has a fuse to show. Alt is only the
  pipette: no wheel gesture is bound to overlay opacity.
- **Cursor mirroring** — the brush cursor and pipette crosshair, wherever
  the pointer sits in one pane, are echoed onto the other pane at the same
  underlying scene point, so the source frame and the composite stay in
  visual sync while editing.

Saving strokes as a checkpoint, restoring an earlier one and branching are
covered in [checkpoints](history.md).

Editing in the window paints the tab's own held Z-Map and recomposites the
composite pane live. Saving the project keeps the edits as checkpoints in
its `.ffp`; **Export image** writes the result as an image, and
`focusforge-cli retouch` writes an edited Z-Map and its
`--render`/`--overlay` outputs to disk.
