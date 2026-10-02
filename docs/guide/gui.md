# Using the window

`focusforge-gui` opens a window instead of running one command and exiting;
everything below is reachable with a mouse once it's open.

```bash
target/release/focusforge-gui --open path/to/frames/
```

A folder argument proposes a grouping the same way `focusforge-cli stack`
would and opens each proposed stack as its own tab; a `*.ffp` path reopens a
project that was already stacked. `--open` is repeatable — each path is
opened in turn, the next one waiting for the one before it to finish rather
than racing it. Leaving `--open` off starts with no project open — use
**Open project…** (below) or drop a folder or a set of frames onto the
window. A path that fails to open shows the same refusal dialog a failed
drop or picker choice would, rather than closing the window.

Opening a folder, a project or a picker/drop result reads its frame headers
and decodes a thumbnail off the window's own thread, so the window keeps
responding while a large folder is scanned; a loading indicator appears over
the canvas once a load has been running for half a second.

## The window

A tab strip runs across the top, one tab per open project plus a `+` tab for
starting another. Below it, the left canvas pane shows the source frame
selected in **Images**; the right pane shows the composite or Z-Map after a
fuse. Both panes pan and zoom together. A dock along the side switches
between two tabs of its own:

- **Images** lists every frame in fuse order, with its legend color and
  capture time. A frame can be excluded from the next fuse without removing
  it, removed outright, re-ordered by dragging its row, and sorted by name or
  capture time. Its header adds more frames through a file picker, and
  dropping a folder or a set of frames onto a tab that already holds a
  project adds them to it the same way, rather than opening a second project;
  a frame that would replace one already in the stack asks before doing so.
  The panel dims while its own tab's fuse is running, the same as
  **Settings**.
- **Settings** lists the alignment and fusion algorithms and the parameters
  each one declares, generated from the same parameter schema
  `focusforge-cli algorithms` prints — choosing a different algorithm or
  editing a value here changes what the next fuse on this tab runs with, and
  a value outside its declared range is flagged before the run starts.

## Fusing a stack

The toolbar's transport button reads **Fuse stack** on a tab with two or
more frames still to fuse; while a run is going it becomes **Pause**,
**Cancel** and, once paused, **Resume**. A failed run offers **Retry**,
which starts the same request again. Progress, the current stage and an ETA
show along the bottom of the window while a run is live. A failed run raises
a toast naming which step it failed at — setup, alignment or fusion —
rather than a generic failure message.

If the graphics device the window is using disappears (a driver reset, a
GPU unplugged), the canvas stops updating and the window closes itself a
moment later rather than hanging; see
[`guide/diagnostics.md`](diagnostics.md) for where that is logged.

## Saving a project

A fuse in the window writes nothing next to the frames: the result is held
in the tab only, and a re-fuse of an already-opened project holds its new
result the same way rather than appending it on disk. A tab with anything
held shows an `UNSAVED` chip in the tab strip; **Save project** (in the
projects overlay's rail, and bound to Ctrl+S) writes every held checkpoint
into one `.ffp`, in order, clearing the chip. On a tab's first save it
proposes `<frames directory>/<group name>.ffp` through a save dialog; a tab
already opened from a project writes back to that same file without asking.
A failed save keeps every held checkpoint and reports why the write did not
happen — nothing is lost.

## Checkpoints

The dock's **CHECKPOINTS** panel lists every checkpoint of the active tab,
newest first. Click a row to restore it, hover one for its delete icon, and
use the editor toolbar's **Save checkpoint** to keep unsaved strokes as a new
row. Fusing, closing a tab and quitting each ask before they would drop
unsaved strokes. See [`guide/history.md`](history.md).

## Exporting an image

The panel's **Export image** button is enabled once the tab shows a fused
or retouched result, and exports the position you are looking at: a saved
checkpoint, an unsaved fuse result, or your current unsaved edits. The
dialog offers:

- **Format** — TIFF, PNG or JPEG.
- That format's settings, listed from the same descriptors `focusforge-cli
  export` takes: bit depth and compression for TIFF and PNG, quality and
  chroma subsampling for JPEG (4:2:0 by default).
- **Metadata** — one switch. On carries the EXIF and ICC profile of the
  reference frame; off carries nothing.

**Export** then opens the system save dialog and writes the image, with
progress shown in the dialog. The format and each format's last settings are remembered in
`settings.toml`'s `[export]` table once an export succeeds. The image is
rendered from the Z-Map, so the source frames are read again and checked;
an export that cannot proceed says why instead of writing, naming each
missing or changed frame, an image too large for the format (JPEG is limited
to 65,535 pixels per side), or a format the image cannot be saved in (PNG
and JPEG hold grey or RGB images only). A refusal for missing or changed
frames offers **Locate frames…**, after which the export is retried at the
same path.

## Retouching a result

Once a tab has fused, the **STACKING / EDITOR** switch in the dock lights up
its **EDITOR** half; switching to it turns the right pane into a paintable
composite with a brush, a pipette and a rainbow source legend. See
[`guide/retouch.md`](retouch.md) for the full workflow.

## Switching between projects

The projects overlay lists every open project as a card, plus a card for any
stack that was proposed but never finished — one for a run that failed, was
cancelled, or whose frames are no longer where the project recorded them.
Clicking an open card switches to that tab; clicking a failed one offers
**Run again**, and a card missing its frames offers **Locate frames…**. A
rail down the side carries the file-level actions — **New project**,
**Open project…**, **Close project**, **Save project** (enabled only once
the active tab holds something unsaved) — and a list of recently opened
projects. Clicking a Recent row opens that project, or switches to its tab if
it is already open.

The four file actions also have fixed shortcuts that work anywhere in the
window, not only with the overlay open: Ctrl+N (**New project**), Ctrl+O
(**Open project…**), Ctrl+S (**Save project**) and Ctrl+W (**Close
project**). They are not part of the rebindable input map, and they do
nothing while the settings panel or a close or quit question is open.

Closing a tab that holds unsaved checkpoints or strokes asks **Save this
state first?** and offers **Save and close** or **Close anyway**. Quitting the
window with unsaved work — including through the window manager, e.g.
Alt+F4 — asks **Quit without saving?** and offers **Save all and quit** or
**Quit without saving**. Dismissing either question leaves everything open.

## Keyboard and mouse bindings

The gear at the bottom of the window opens a settings panel whose **Input
map** section lists every stacking and editor action alongside the input
bound to it, and where that binding came from — the shipped defaults or the
settings file. The bindings live in the `[input]` table of `settings.toml`,
edited by hand rather than through the window; the panel is the read-only
view of what took effect. A table with any problem is refused whole and the
defaults apply, with the problem named: an unknown trigger or action, a
duplicate trigger, or a trigger the map has no row for such as `alt+wheel`.
Alt is not a wheel modifier; it is only the pipette.

## The settings file

`settings.toml` in the platform's per-user config directory holds
everything the window remembers, one table per concern: `[recent]` (the
rail's recent projects, up to five, each row needing its `path`), `[input]`
(the bindings above) and `[export]` (the export dialog's last choice). The
file has no version. Writing one table leaves the others, and any comments
in them, as they were; a file that is not valid TOML is left untouched
rather than overwritten. `--config-dir <DIR>` makes the window read and
write `<DIR>/focusforge/settings.toml` instead.
