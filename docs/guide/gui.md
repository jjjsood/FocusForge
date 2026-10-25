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
than racing it. Leaving `--open` off starts with an empty window: click
**Open…** to choose the TIFF or camera RAW frames of one stack, which start
one new tab (sorted by name), or drop a folder or a set of frames onto the
window. The empty window's caption reads "Drop a folder or stack", or
"Dropping is unavailable on native Wayland" where drops do not work.
Ctrl+N and the Projects rail's **New project** ask for a folder instead. A path that fails to open shows the same refusal dialog a failed
drop or picker choice would, rather than closing the window. A folder with
no readable frames is refused naming the formats read, 16-bit TIFF and camera
RAW; Escape closes the dialog. Dialogs with a Cancel button also show an x at
the top right that acts as Cancel.

Opening a folder, a project or a picker/drop result reads its frame headers
and decodes a thumbnail off the window's own thread, so the window keeps
responding while a large folder is scanned, and while an open, save or
export file dialog is up; a loading indicator appears over
the canvas once a load has been running for half a second.

On Linux, a launch that carries a startup-notification token
(`DESKTOP_STARTUP_ID` or `XDG_ACTIVATION_TOKEN`) tells the desktop once the
window has appeared, so the launcher's busy cursor ends.

## Launch options

Other `focusforge-gui` flags start the window in a chosen state; `--help`
lists them all.

| flag | effect |
|---|---|
| `--fuse-tabs 0,1` | selects those tabs in turn and presses **Fuse stack** on each once `--open` has loaded, so give one `--open <folder>` per stack (a folder that reads as several stacks first asks how to group them, and until that is answered no tab exists for the flag to select); an index repeated, as in `0,0`, waits until the earlier run has settled and its **Keep** or **Discard** question is answered |
| `--projects-overlay` | opens the projects overlay; with `--open` it stays open after the load lands, with `--fuse-tabs` it opens once the last run settles |
| `--toolbar-state idle\|running\|paused\|cancelling\|failed` | shows that transport state until a real run starts |
| `--save-folder <DIR>` | writes every project save and image export into this existing folder under its proposed name, with no save dialog |
| `--settings-section <TAB>` | opens the settings window on that tab (see below) |
| `--config-dir <DIR>` | reads and writes `<DIR>/focusforge/settings.toml` (see below) |

## The window

A tab strip runs across the top, one tab per open project plus a `+` tab for
starting another. Below it, the left canvas pane, **Frame**, shows the source frame
selected in **Images**; the right pane, **Composite**, shows the result after
a fuse. Both panes pan and zoom together. A dock along the side switches
between two tabs of its own:

- **Images** lists every frame in fuse order, with its legend color and
  capture time. A frame can be excluded from the next fuse without removing
  it, removed outright, re-ordered by dragging its row, and sorted by name or
  capture time. Its header adds more frames through a file picker, and
  dropping a folder or a set of frames onto a tab that already holds a
  project adds them to it the same way, rather than opening a second project;
  a frame that would replace one already in the stack asks before doing so.
  The panel dims while its own tab's fuse is running, the same as
  **Settings**. The Appearance tab of the settings window sets whether the panel shows each frame as a
  plain row, a row with a preview, or a large preview. The editor's source
  list follows the same choice.
- **Settings** holds every setting of the next fuse on this tab, in four
  sections; see [Settings](#settings) below. A value outside its declared
  range is flagged before the run starts.

Use the icon at the top right of either image to move that pane into a second
window. Only one pane can be detached, and the remaining pane's icon stays
hidden until the detached pane returns. The remaining pane fills the main
canvas. Both images share the same visible area, zoom and pan; unused space
stays black when the windows have different sizes. Each pane, the detached
window included, shows a coarse whole-image backdrop under the sharp view, so
zooming out or panning past the loaded region shows no blank edge while the
sharp pixels catch up. Drag the second window by
its title bar; its dock icon or close button returns the image to the main
window. It can also be minimized or maximized. It draws sharply over its
whole area and follows the display's scale factor. On Linux, the second window
currently uses X11 or XWayland when `DISPLAY` is available, otherwise native
Wayland when `WAYLAND_DISPLAY` is available. Starting with neither display
reports a clear error instead of choosing a different backend.

## Staying responsive

Loading frame planes, saving a project, loading one and blending
a brush stroke all run off the window's thread, so the window keeps
responding while they work. While you pan, zoom or paint, a running fuse or
alignment holds back to a quarter of the machine's threads (at least one) and
lifts that cap once the canvas has been quiet for a quarter of a second.

## Fusing a stack

The toolbar's transport button reads **Fuse stack** on a tab with two or
more frames still to fuse; while a run is going it becomes **Pause**,
**Cancel** and, once paused, **Resume**. A failed run offers **Retry**,
which starts the same request again. Progress, the current stage and an ETA
show along the bottom of the window while a run is live. Brightness
preparation and a computed alignment each show frames done and an estimated
time remaining; time spent paused is left out of the estimate. When alignment
is reused from an earlier run, the bar reads "from cache" for that stage. A failed run raises
a toast naming which step it failed at — setup, alignment or fusion —
rather than a generic failure message.

If the graphics device the window is using disappears (a driver reset, a
GPU unplugged), the canvas stops updating and the window closes itself a
moment later rather than hanging; see
[`guide/diagnostics.md`](diagnostics.md) for where that is logged.

## Settings

The **Settings** tab is generated from the same parameter schema
`focusforge-cli algorithms` prints. It has four sections, top to bottom:

- **PREPARE** — **Equalize brightness** (on by default), **Max brightness
  gain** (1.01 to 2.0, default 1.25) and **Target brightness (stops)**, a
  signed slider from -0.5 to 0.5 in steps of 0.1 (Left and Right step it from
  the keyboard). Every frame gets one gain, bringing its mean to one target
  shared by all frames, bounded by the max gain; the target sits at the
  frames' average brightness at 0.0 and a stop higher or lower doubles or
  halves it. While the gains are measured the bottom bar reads **Preparing** with the number of frames done and an
  estimated time remaining.
- **FUSE** — the fusion method and the parameters it declares. The picker
  lists every method: **Details** (the default), **Soft transitions** and
  **Focus map**. Each method remembers its own settings while the tab is open.
- **ALIGN** — an **Enabled** switch, then the alignment algorithm and its
  parameters. Switched off, the section dims and the next fuse uses the
  frames as read.
- **FINISH** — **Auto-crop**, which trims the result to the area every
  aligned frame covers.

A section's heading carries a **DEFAULT** badge while nothing in it differs
from the defaults; in **ALIGN** the **Enabled** switch counts, so switching
alignment off or on removes the badge. Rarely changed parameters sit behind a **More...** strip at the foot of a
section; click it to open the strip and click again to collapse it. Choosing a
different method or editing a value changes what the next fuse on this tab
runs with.

A fuse does not stop at a frame that fails to register: the run finishes with
that frame unaligned and offers **Keep** or discard. After **Keep**, an info
toast reads `N frames unaligned` (`1 frame unaligned`) when any frame fell
back to identity. The same toast carries the run's other notices, such as a
frame whose brightness gain was held at the bound. A re-fuse that changes only
fusion settings reuses the stored alignment. See
[`guide/alignment.md`](alignment.md) and
[`guide/stacking.md`](stacking.md#brightness-equalization).

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
rendered from the stored composite with any strokes blended over it; a
retouched result reads the source frames again and checks them. An export that cannot proceed says why instead of writing, naming each
missing or changed frame, an image too large for the format (JPEG is limited
to 65,535 pixels per side), or a format the image cannot be saved in (PNG
and JPEG hold grey or RGB images only). A refusal for missing or changed
frames offers **Locate frames…**, after which the export is retried at the
same path.

## Retouching a result

Once a tab has fused, the **STACKING / EDITOR** switch in the dock lights up
its **EDITOR** half; switching to it turns the **Composite** pane into a paintable
result with a brush, a pipette and a rainbow source legend. See
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
Escape is fixed the same way: it closes a refusal dialog first, otherwise the
projects overlay, and closes the settings window from any of its tabs.

Closing a tab that holds unsaved checkpoints or strokes asks **Save this
state first?** and offers **Save and close** or **Close anyway**. Quitting the
window with unsaved work — including through the window manager, e.g.
Alt+F4 — asks **Quit without saving?** and offers **Save all and quit** or
**Quit without saving**. Dismissing either question leaves everything open.

## Notifications

Everything the window has to tell you outside a dialog is kept as a
notification for the rest of the session: a failed run or save, a processing
notice such as a held brightness gain, skipped subfolders on an open and a
notice about crash reports left by an earlier session. Nothing is written to
disk, so closing the window ends the history.

A bell sits in the icon bar down the left edge, just above the **Settings** icon. Its badge
counts the notifications you have not read and disappears at zero. Clicking
the bell opens the **Notifications** center beside the bar; opening it marks
every notification read, and it stays reachable with no project open and over
the projects overlay (which it closes first). Each row shows a title, a
context line with the source, project, file and time; select a row to read
its full message.

A new notification that arrives while the window is in front also appears as a
toast: six seconds for most, ten for a crash notice, one at a time with the
rest queued. Hovering a toast or focusing it from the keyboard pauses its
timer, and clicking it opens the center on that notification. While the center
or the projects overlay is open no toast is shown; what arrives meanwhile is
kept, counted on the badge and never replayed as a toast.

### Banners while the window is in the background

When every FocusForge window is behind other applications, a new notification
also appears once as a banner from your operating system, carrying only its
title and a one-line context. The full message and any file paths stay in the
center, which marks them read when you return to it. Switching between the
main window and the detached pane does not count as leaving, no banner repeats
when you come back, and none is shown while the projects overlay is open.
Dismissing a banner, or the system refusing it, changes nothing in the app: the
notification stays in the center and on the badge, and a send that fails is
logged once and not retried.

Banners use the identity `org.focusforge.FocusForge`. On Linux they go through
the desktop's notification service, which attributes them to FocusForge when
`assets/focusforge.desktop` is installed as
`~/.local/share/applications/focusforge.desktop`. Without a running
notification service there is no banner and everything else works as usual.

## The settings window

The **Settings** icon at the foot of the icon bar opens the settings window,
which has six tabs:

- **Performance** switches GPU processing on or off, shows the backend the
  last run resolved, and takes an optional **Host memory cap** and **VRAM
  cap** in the size syntax of `--max-host-memory` and `--max-vram` (for
  example `8GiB`). A cap only lowers a budget; the **Host budget** and
  **VRAM budget** rows show the budgets in effect. A field holding an
  unparsable size is flagged. A changed VRAM cap applies from the next start.
- **Appearance** chooses how the **Images** panel presents the source frames:
  **List**, **Image list** (the default) or **Large previews**. The latter two
  show a preview of each original source image beside its name. The choice is
  saved in the `[appearance]` table of `settings.toml`.
- **Storage** lists where the settings file, the logs, the crash reports and
  the scratch directory live, and marks one that does not exist yet. **Clear
  cache** deletes the cached fuse stages.
- **Input map** lists every stacking and editor action alongside the input
  bound to it, and where that binding came from: the shipped defaults or the
  settings file. See [Keyboard and mouse bindings](#keyboard-and-mouse-bindings).
- **Diagnostics** shows the file list a diagnostics bundle would hold and
  creates it; see [`guide/diagnostics.md`](diagnostics.md).
- **About** shows the version, the licence (GPL-3.0), the copyright and
  warranty notice, and the source repository.

`focusforge-gui --settings-section performance` opens the window on that tab
at startup. The other accepted sections are `appearance`, `storage`,
`input-map`, `diagnostics` and `about`.

## Keyboard and mouse bindings

The **Input map** tab is the read-only view of the bindings that took
effect. They live in the `[input]` table of `settings.toml`,
edited by hand rather than through the window. A table with any problem is
refused whole and the defaults apply, with the problem named: an unknown
trigger or action, a duplicate trigger, or a trigger the map has no row for
such as `alt+wheel`. Alt is not a wheel modifier; it is only the pipette.

## The settings file

`settings.toml` in the platform's per-user config directory holds
everything the window remembers, one table per concern: `[recent]` (the
rail's recent projects, up to five, each row needing its `path`), `[appearance]`
(`presentation = "list"|"image-list"|"large"`), `[input]`
(the bindings above), `[export]` (the export dialog's last choice) and
`[performance]` (`gpu = true|false`, and optional `max-host-memory` and
`max-vram` size strings, as set on the Performance tab). The
file has no version. Writing one table leaves the others, and any comments
in them, as they were; a file that is not valid TOML is left untouched
rather than overwritten. `--config-dir <DIR>` makes the window read and
write `<DIR>/focusforge/settings.toml` instead.
