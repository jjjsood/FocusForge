# Checkpoints

A project keeps its history as a tree of checkpoints. Every fuse produces
one, and every set of retouch edits you save adds one on top of the
checkpoint it started from. Any checkpoint can be restored, exported or
branched from, so an experiment never costs the version you already liked.
Because a checkpoint is a Z-Map, not pixels, a retouched checkpoint adds
only the size of its edit to the project file.

## In the window

The **CHECKPOINTS** panel in the dock lists the active tab's checkpoints,
newest first, grouped under day headings. Each row shows the algorithm and
the time it was made.

- **Restore** — click a row. The tab shows that checkpoint, and edits made
  from here continue from it. Clicking an older row and painting creates a
  branch.
- **Save checkpoint** — the editor toolbar's button keeps the strokes you
  have made as a new checkpoint at the top of the list. The new row flashes
  briefly. The button is enabled while there are unsaved strokes.
- **Delete** — hover a row and click its trash icon. The dialog names the
  checkpoint and says the others are untouched. The last remaining
  checkpoint cannot be deleted.
- **Export image** — the panel header's button exports the checkpoint you
  are looking at; see [exporting](gui.md#exporting-an-image).

A fuse result and saved strokes are held in the tab until you save the
project; the tab shows an `UNSAVED` chip, and **Save project** (Ctrl+S)
writes all of them into the `.ffp` together. Deleting a checkpoint is
likewise applied when the project is saved.

Strokes that belong to no checkpoint yet are never dropped silently:

- Restoring another row asks **Save this state first?** and offers **Save
  first, then restore** or **Discard and restore**.
- Fusing again asks **Save your edits first?** with **Save and fuse** or
  **Fuse anyway**.
- Closing the tab or quitting the window asks before discarding the strokes
  and any unsaved checkpoints; see [the window guide](gui.md#switching-between-projects).

## From the command line

```bash
focusforge-cli history list frames.ffp
focusforge-cli history apply frames.ffp --ops ops.json --save
focusforge-cli history apply frames.ffp --ops ops.json --from 5b71 --save
focusforge-cli history materialize frames.ffp --checkpoint 5b71 -o zmap.tif
focusforge-cli history delete frames.ffp --checkpoint 5b71
```

- `list` prints every visible checkpoint, newest first, with its parent.
  Ids can be shortened to any unique prefix everywhere.
- `apply` replays an op list (`{"ops": [...]}`, the shape in
  [retouching](retouch.md)) onto a checkpoint, the head unless `--from`
  names another. `--save` seals the result as a new checkpoint; with `-o`
  it also writes the edited Z-Map, and with `-o` alone it only previews
  without changing the project. Applying onto an older checkpoint starts a
  branch.
- `materialize` writes a checkpoint's Z-Map as a TIFF.
- `delete` hides a checkpoint and removes it from the file once no visible
  checkpoint is built on it. The last visible checkpoint is refused.

`export --checkpoint <id>` renders any checkpoint to an image.

## Damaged or moved projects

A project is checked when it is opened and a damaged one is refused by name
instead of being opened partly; the exit codes are in
[the CLI reference](../reference/cli.md#history). A project written by an
older build that lacks the records the current one needs is refused, and
re-stacking the frames produces a current project.

If the source frames were moved or changed since the project was written,
the window asks before opening and offers **Locate frames…**, which accepts
a folder only when its files hash to what the project recorded. On the
command line, `--accept-changed-sources` proceeds with a warning. Details:
[the project format](../reference/project-format.md#source-frames).
