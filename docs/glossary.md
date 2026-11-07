# Glossary

The words this manual uses, in alphabetical order. Each entry says in a
sentence or two what the word means in FocusForge and links to the section
that explains it properly. Labels you click in the window are in **bold**.

## Alignment

Moving every frame of a stack so that it lines up with one
[reference frame](#reference-frame) before the frames are fused. It undoes
camera drift and [focus breathing](#focus-breathing); it is on by default,
and your frame files are never changed.
See: [What alignment does](guide/alignment.md#what-alignment-does).

## Auto-crop

The **FINISH** setting that trims the result to the area every aligned frame
covers, so the edges are clean. It is why a result is usually a few pixels
smaller than the frames. On the command line, `--no-crop` keeps the full
frame.
See: [FINISH: trim the edges](guide/stacking.md#finish-trim-the-edges).

## Batch run

A command-line run that stacks a whole folder holding several stacks in one
go, one project per stack.
See: [Batch runs from the command line](reference/batch-runs.md).

## Branch

A [checkpoint](#checkpoint) built on an older checkpoint rather than on the
newest one. Checkpoints form a tree, so you can try a second repair without
losing the first.
See: [Branch from an older checkpoint](guide/history.md#branch-from-an-older-checkpoint).

## Brightness equalization

The **PREPARE** step that gives every frame one brightness factor (a gain),
so all frames share the same average brightness before they are aligned and
fused. It is on by default; **Max brightness gain** (1.25) limits how far a
frame may be changed.
See: [Brightness equalization](guide/stacking.md#brightness-equalization).

## Brightness target

**Target brightness (stops)**: an optional shift, up to half a stop, that
makes the whole stack a little brighter or darker during the fuse. It works
within the **Max brightness gain** limit.
See: [Set a brightness target](guide/stacking.md#set-a-brightness-target).

## Brush stroke

One drag of the brush in the [editor](#editor). A stroke paints the pixels of
the chosen [source frame](#source-frame) over the [composite](#composite)
where you brush. Strokes are kept as a list of edits, so you can undo them
and the fused result itself is never destroyed.
See: [Paint a stroke](guide/retouch.md#paint-a-stroke).

## Budget

The amount of memory FocusForge allows itself for a fuse: the host
memory budget (40 % of the memory free when a run starts) and the VRAM budget
on the graphics card, measured the first time a fuse uses it. FocusForge
works through a stack in pieces so that it stays within its budget.
See: [Memory caps and budgets](guide/settings-window.md#memory-caps-and-budgets).

## Camera RAW

The unprocessed files a camera records (`.cr3`, `.nef`, `.arw`, DNG and
many more). FocusForge opens a folder of RAW files like a folder of TIFF
files and develops them once into the camera's own linear colour, using each
file's recorded white balance. The developed frames, the fused result and a
RAW project's saved composite stay unclipped, so highlights above white
survive until the result is exported. One stack is always one kind of file.
See: [Camera RAW, JPEG and compressed TIFF](guide/first-stack.md#camera-raw-jpeg-and-compressed-tiff)
and [Choose a file format](stacking/shooting-practice.md#choose-a-file-format).

## Cap

A memory ceiling you set yourself on top of a [budget](#budget): the **Host
memory** and **VRAM** limits. A cap can only lower a budget, never raise
it; a lower cap can make a fuse slower but never changes the result.
See: [Memory caps and budgets](guide/settings-window.md#memory-caps-and-budgets).

## Checkpoint

A version of a project's result you can go back to. Every fuse you keep adds
one, and so does every set of strokes you keep with **Save checkpoint**.
Checkpoints are listed under **CHECKPOINTS** in the dock and stored in the
project file.
See: [Checkpoints and saving](guide/history.md).

## Circle of confusion

The largest blur disc that still counts as sharp. It is a convention: the
classic full-frame value of **0.03 mm** describes a whole picture viewed at a
normal size, and the [depth of field chapter](stacking/depth-of-field.md)
uses it. [Shooting a stack](stacking/shooting-practice.md) uses
**0.015 mm**, because a stack is usually judged at 100 % on screen; its depth
of field and step figures are therefore half the classic ones.
See: [What depth of field is](stacking/depth-of-field.md#what-depth-of-field-is)
and [The model behind the numbers](stacking/shooting-practice.md#the-model-behind-the-numbers).

## Composite

The single image a fuse makes from the frames of a stack: the result you
retouch and export. The window shows it in the **Composite** pane.
See: [What focus stacking does](stacking/depth-of-field.md#what-focus-stacking-does)
and [The two panes](guide/gui.md#the-two-panes).

## Crash report

The evidence FocusForge writes when it stops unexpectedly: the error, where
it happened and the most recent log activity, or a [minidump](#minidump)
after a hard crash. It stays on your computer; the next start tells you about
it with the notice "Previous session stopped".
See: [Crash reports](guide/diagnostics.md#crash-reports).

## Depth of field

The zone in front of and behind the plane of focus in which the blur stays
smaller than the [circle of confusion](#circle-of-confusion), so the picture
looks sharp. At macro distances it is a fraction of a millimetre, which is
why stacks exist.
See: [What depth of field is](stacking/depth-of-field.md#what-depth-of-field-is).

## Developing

The first stage of a fuse of camera RAW frames: each frame is converted once
from the camera's recorded data to linear colour, and the later stages work
on that. The bottom bar reads **Developing** with the number of frames done.
TIFF and JPEG frames need no developing, so their fuse starts at
**Preparing**.
See: [Fuse the stack](guide/first-stack.md#fuse-the-stack).

## Details

The default [fusion method](#fusion-method) (`laplacian` on the command
line). It splits every frame into layers from coarse to fine (a
[pyramid](#pyramid)) and takes the sharpest frame's contribution in each
layer. It keeps the most fine detail and is the fastest, but can leave a
[halo](#halo) at strong depth edges.
See: [Details](guide/stacking.md#details).

## Diagnostics bundle

One ZIP file holding FocusForge's logs, any [crash reports](#crash-report)
and a description of your system, with your home folder and user name
replaced. You make it yourself, from the settings window's **Diagnostics**
section, and attach it to a bug report; nothing is ever sent automatically.
See: [Make a diagnostics bundle](guide/diagnostics.md#make-a-diagnostics-bundle).

## Diffraction

The softening of fine detail that grows as you stop the lens down. At macro
magnifications it sets in earlier than the f-number on the lens suggests,
because of the [effective aperture](#effective-aperture).
See: [Diffraction](stacking/shooting-practice.md#diffraction) and
[Why stopping down is not the answer](stacking/depth-of-field.md#why-stopping-down-is-not-the-answer).

## Dock

The panel down the right side of the window: the **STACKING** / **EDITOR**
switch, the **CHECKPOINTS** list and, in stacking, the **Images** and
**Settings** tabs.
See: [The dock](guide/gui.md#the-dock).

## Editor

The window's **EDITOR** mode, in which you retouch a fused result with the
brush. It becomes available once a project holds a fused result.
See: [Switch to the editor](guide/retouch.md#switch-to-the-editor).

## Effective aperture

The f-number a lens behaves like when it focuses close: about N × (1 + m),
for the marked f-number N and the [magnification](#magnification) m. At 1:1,
f/8 on the lens works like f/16.
See: [Effective aperture](stacking/shooting-practice.md#effective-aperture).

## Exclude

Leaving a frame out of the next fuse without removing it from the project,
with **Exclude** in the **Images** panel.
See: [Leave a frame out of the next fuse](guide/images-panel.md#leave-a-frame-out-of-the-next-fuse).

## F-number

The number that names how far a lens is opened, written f/8: the focal length
divided by the diameter of the opening. A larger number is a smaller opening,
which gives a deeper zone of sharpness and lets in less light. Each
[stop](#stop) changes it by a factor of about 1.4.
See: [What makes it deep or shallow](stacking/depth-of-field.md#what-makes-it-deep-or-shallow).

## `.ffp`

The extension of a FocusForge [project](#project) file. One `.ffp` file holds
the list of source frames and every [checkpoint](#checkpoint); it does not
contain the frames themselves.
See: [What you need](guide/first-stack.md#what-you-need) and
[The project container](reference/project-format.md).

## Focus breathing

The slight change in image size many lenses show as the focus moves. Left
alone it doubles edges in a stack; alignment's default **Rotation + Scale**
mode corrects it.
See: [What alignment absorbs](stacking/shooting-practice.md#what-alignment-absorbs)
and [Alignment](guide/alignment.md).

## Focus gap

A slice of the subject that is sharp in none of the frames, because the
[step](#step) between two frames was larger than their depth of field. It
shows as a soft band across the result and cannot be repaired from the
frames you have.
See: [Focus gaps](stacking/failure-images.md#focus-gaps).

## Focus map

A [fusion method](#fusion-method) (`depth-map` on the command line). It
builds a map of which frame is sharpest where, smooths it, and takes each
pixel from the one frame the map names, giving the most orderly
[Z-Map](#z-map).
See: [Focus map](guide/stacking.md#focus-map).

## Focus stacking

Taking several photographs of a subject, each focused at a different depth,
and combining the sharp parts of all of them into one image that is sharp
from front to back.
See: [What focus stacking does](stacking/depth-of-field.md#what-focus-stacking-does).

## Frame

One photograph of a [stack](#stack). The frames of one stack sit in one
folder and are read in file-name order.
See: [What you need](guide/first-stack.md#what-you-need).

## Fuse

To combine the frames of a stack into one [composite](#composite): click
**Fuse stack**. A fuse runs through preparing, aligning and fusing; the
result it keeps becomes a new [checkpoint](#checkpoint).
See: [Fuse the stack](guide/first-stack.md#fuse-the-stack).

## Fusion method

The rule that decides, for every pixel, which frames the result takes it
from. The window offers three: [Details](#details),
[Soft transitions](#soft-transitions) and [Focus map](#focus-map).
See: [Choose a fusion method](guide/stacking.md#choose-a-fusion-method).

## GPU processing

Fusing on the graphics card. It is usually faster and gives the same result
as the processor (within one step of the 16-bit scale). When no usable
graphics card is found, FocusForge uses the processor instead.
See: [GPU processing and the Backend line](guide/settings-window.md#gpu-processing-and-the-backend-line).

## Grouping

How FocusForge splits a folder into stacks: by subfolder, and within a
folder by a gap of more than a minute in the frames' capture times. The
window asks before it opens a folder as several projects.
See: [A folder with several stacks](guide/first-stack.md#a-folder-with-several-stacks).

## Halo

A pale or smeared fringe along a depth edge, where a near outline stands
against a far background. It comes from the optics, not from a bug, and can
be reduced by the choice of [fusion method](#fusion-method) or painted away in
the editor.
See: [Halos at depth edges](stacking/failure-images.md#halos-at-depth-edges)
and [Example: fix a halo](guide/retouch.md#example-fix-a-halo).

## Identity fallback

What happens to a frame that cannot be aligned: it is used unmoved, exactly
as it was shot. In the window the fuse then asks **Keep this fused result?**
and names the frames.
See: [When a frame cannot be aligned](guide/alignment.md#when-a-frame-cannot-be-aligned).

## Magnification

The size of the subject's image on the sensor divided by its real size. 1:1
("life size") is a magnification of 1. In macro work it is the factor that
shrinks the [depth of field](#depth-of-field) most.
See: [What makes it deep or shallow](stacking/depth-of-field.md#what-makes-it-deep-or-shallow)
and [Know your magnification](stacking/shooting-practice.md#know-your-magnification).

## Minidump

A snapshot of a crashed program's memory, written by FocusForge's watcher
process after a hard crash. It cannot be cleaned of personal paths, so a
[diagnostics bundle](#diagnostics-bundle) includes it only when you switch on
**Include minidumps**.
See: [Crash reports](guide/diagnostics.md#crash-reports) and
[Include minidumps](guide/diagnostics.md#include-minidumps).

## Motion

Movement of the camera or the subject between frames. It shows as doubled
outlines or structures that do not meet; [alignment](#alignment) repairs
camera movement, not a subject that moved.
See: [Motion between frames](stacking/failure-images.md#motion-between-frames).

## Notification

A message the window keeps for the session in the notification center behind
the bell: a failed fuse or save, a notice from a run, skipped folders, a
crash. A new one also shows briefly as a [toast](#toast), or as a desktop
banner when the window is in the background.
See: [Notifications](guide/batch-runs.md#notifications).

## Overlap

The share of one frame's depth of field that its neighbour covers as well.
An overlap of 25 % is a good default; it keeps small focusing errors from
opening a [focus gap](#focus-gap).
See: [Step and overlap](stacking/shooting-practice.md#step-and-overlap).

## Pane

One of the two image areas of the window: **Frame** on the left, showing one
source frame, and **Composite** on the right, showing the result. Either can
move into a window of its own.
See: [The two panes](guide/gui.md#the-two-panes).

## Project

One stack with everything you did to it: its frames, settings and
[checkpoints](#checkpoint). Each open project has its own [tab](#tab);
saved, it is one [`.ffp`](#ffp) file.
See: [What you need](guide/first-stack.md#what-you-need) and
[Projects, notifications and quitting](guide/batch-runs.md).

## Projects overview

The grid of project cards that opens from the button at the left end of the
tab strip. It shows every open project, the ones that need attention, and
the recent ones.
See: [The Projects overview](guide/batch-runs.md#the-projects-overview).

## Pyramid

A series of ever smaller, coarser copies of an image. The
[Details](#details) method blends frames layer by layer of such a pyramid,
and [alignment](#alignment) finds the movement on the small copies first.
See: [Details](guide/stacking.md#details) and
[What alignment does](guide/alignment.md#what-alignment-does).

## Reference frame

The frame the others are aligned to. By default it is the middle frame of
the stack.
See: [Choose the reference frame](guide/alignment.md#choose-the-reference-frame).

## Soft transitions

A [fusion method](#fusion-method) (`weighted-average` on the command line,
which prints it as "Soft transitions"). Instead of picking one frame per pixel
it blends all of them, giving sharper frames more weight.
See: [Soft transitions](guide/stacking.md#soft-transitions).

## Source frame

One of the original frames a project was made from. In the editor, it is the
frame you choose to paint from. A project recognises its source frames by
their location and a fingerprint of their content.
See: [Choose the frame to paint from](guide/retouch.md#choose-the-frame-to-paint-from)
and [Damaged or moved projects](guide/first-stack.md#damaged-or-moved-projects).

## Stack

The series of frames of one subject, each focused a little deeper than the
last. In FocusForge one stack becomes one [project](#project).
See: [What you need](guide/first-stack.md#what-you-need) and
[Shooting a stack](stacking/shooting-practice.md).

## Stage cache

Where FocusForge keeps the results of expensive steps of a fuse, above all
the alignment, so that fusing the same frames again can skip them. **Clear
cache** in the settings window empties it; nothing else is affected.
See: [Clear the stage cache](guide/settings-window.md#clear-the-stage-cache).

## Step

The distance the focus moves between two frames. A step no larger than the
depth of field minus the [overlap](#overlap) keeps the stack free of
[focus gaps](#focus-gap).
See: [Step and overlap](stacking/shooting-practice.md#step-and-overlap)
and [Step size](stacking/shooting-practice.md#step-size).

## Stop

One step on the aperture scale (f/2.8, f/4, f/5.6, f/8, f/11, f/16). Moving
one stop doubles or halves the light that reaches the sensor, and changes the
[f-number](#f-number) by a factor of about 1.4.
See: [Effective aperture](stacking/shooting-practice.md#effective-aperture).

## Tab

Each open [project](#project) has a tab in the strip across the top of the
window, with its name, an **x** to close it and, when needed, the
**UNSAVED** chip.
See: [Tabs: one project each](guide/gui.md#tabs-one-project-each).

## TIFF

An image file format FocusForge stacks besides camera RAW and JPEG: 8-bit or
16-bit TIFF, uncompressed or LZW or Deflate compressed. 32-bit floating-point
TIFF, BigTIFF and TIFF compressed with JPEG are not read. Uncompressed 16-bit
TIFFs open fastest.
See: [Choose a file format](stacking/shooting-practice.md#choose-a-file-format)
and [TIFF fast path](reference/tiff-fast-path.md).

## Toast

The short pop-up at the top left of the canvas that shows a new
[notification](#notification) while the window is in front. It disappears
after a few seconds; the notification stays in the center.
See: [Toasts](guide/batch-runs.md#toasts).

## Z-Map

The record, for every pixel of the composite, of which frame it came from.
The **Overlay** switch shows it in colour over the result; the editor and
retouching depend on it, which is why a project checks that its frames are
still the same ones.
See: [See where each pixel came from](guide/gui.md#see-where-each-pixel-came-from)
and [See which frame each area came from](guide/retouch.md#see-which-frame-each-area-came-from).
