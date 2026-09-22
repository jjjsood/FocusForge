# Using the window

`focusforge-gui` opens a window instead of running one command and exiting;
everything below is reachable with a mouse once it's open.

```bash
target/release/focusforge-gui --open path/to/frames/
```

A folder argument proposes a grouping the same way `focusforge-cli stack`
would and opens each proposed stack as its own tab; a `*.ffp` path reopens a
project that was already stacked. Leaving `--open` off starts with no
project open — use **Open project…** (below) or drop a folder or a set of
frames onto the window.

## The window

A tab strip runs across the top, one tab per open project plus a `+` tab for
starting another. Below it, the canvas shows the active tab's composite and
its Z-Map side by side; both panes pan and zoom together. A dock along the
side switches between two tabs of its own:

- **Images** lists every frame in fuse order, with its legend color and
  capture time. A frame can be excluded from the next fuse without removing
  it, removed outright, re-ordered by dragging its row, and sorted by name or
  capture time.
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
show along the bottom of the window while a run is live. Fusing a tab that
was already opened from a project appends the new result rather than
replacing it.

If the graphics device the window is using disappears (a driver reset, a
GPU unplugged), the canvas stops updating and the window closes itself a
moment later rather than hanging; see
[`guide/diagnostics.md`](diagnostics.md) for where that is logged.

## Switching between projects

The projects overlay lists every open project as a card, plus a card for any
stack that was proposed but never finished — one for a run that failed, was
cancelled, or whose frames are no longer where the project recorded them.
Clicking an open card switches to that tab; clicking a failed one offers
**Run again**, and a card missing its frames offers **Locate frames…**. A
rail down the side carries the file-level actions — **New project**,
**Open project…**, **Close project** — and a list of recently opened
projects.

## Keyboard and mouse bindings

The gear at the bottom of the window opens a settings panel whose **Input
map** section lists every stacking and editor action alongside the input
bound to it, and where that binding came from — the shipped defaults or a
config file. The bindings themselves live in `input.toml`, next to the
window's other configuration, and are edited by hand rather than through the
window; the panel is the read-only view of what took effect.
