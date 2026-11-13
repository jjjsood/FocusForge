# Changelog

This file records user-visible changes. Its structure follows [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-11-13

### Changed

- The Details fusion method (`laplacian`) judges sharpness over a smaller area, so fine detail next to stronger detail is taken from the frame in which it is sharp. Fusing a stack again with Details can give a slightly different result and Z-Map than an earlier version.
- Alignment is faster, most on a processor with several cores: with the default Chain reference policy, several frames are compared with their neighbours at once. An alignment that stops improving now ends as `stalled` before Max Iterations run out, so a frame's position can differ slightly from an earlier version.
- Camera RAW stacks fuse faster, most of all on a computer with little free RAM, and give the same result as before. When the developed frames fit in the memory a fuse may use, they stay in memory and the run writes no temporary files for them.
- Long stacks of compressed TIFF frames that store one row per strip fuse faster.
- With no Host memory limit set, a fuse on a computer with plenty of free RAM may use more of it than before. A camera RAW stack is refused only when the memory a fuse may use cannot hold one of its frames.
- When a frame does not fit in the memory a fuse may use, the message names the file, gives the sizes in MB, says whether free RAM or the Host memory limit is too low, how much more is needed and what to do. With very little free RAM it says how much free RAM the run needs in all.
- The window's bottom bar and the progress lines of `focusforge-cli` no longer show an estimated time remaining.
- Long frame names in the Images panel and in Analytics are shortened at their start, so frames whose names differ only at the end stay apart, and the last row of Analytics and of the notification center no longer sits against the edge.

### Fixed

- On a machine without a hardware GPU (a virtual machine, a Remote Desktop session), stacking runs on the CPU instead of a slow software graphics adapter.
- On large frames, alignment on the graphics card finds the same positions as alignment on the processor; before, it could use up Max Iterations and stop a fraction of a pixel away.
- `focusforge-cli align` reads folders of JPEG and camera RAW frames as `stack` does; before, it found only TIFF frames.

## [0.2.0] - 2026-11-09

### Added

- Frame sets of 8-bit TIFF files open and stack; their samples are widened to the 16-bit
  range.
- JPEG frames (`.jpg`, `.jpeg`) open, import and stack in `focusforge-cli` and the GUI. Their
  EXIF capture time groups a folder, and an export from JPEG frames carries their EXIF.
- A step calculator plans a shoot: from the lens f-stop or the objective's numerical aperture, the magnification, the overlap and the depth range it gives the depth of field, the step between frames, the frame count, the duration and the disk need. It opens from a button in the Stacking window, with or without a project, and runs as `focusforge-cli step-plan`.
- Analytics counts the sharp areas of every frame, in the background and kept per stack. It opens from a button in the Stacking window, also without a stack, and runs as `focusforge-cli sharp-areas`.
- FocusForge checks once at start whether a newer final release exists and shows a notice with the link to the download page. It never downloads or installs anything and fails silently. Settings has the start-up switch (on by default), Check now and Skip this version; `focusforge-cli check-update` runs the same check.
- The Composite pane shows a fusing run live: the frame just aligned, a sharpness heatmap and the result growing tile by tile.
- The app's transitions are animated; Reduce motion in Settings turns the animation off.
- The progress of a RAW stack shows the develop pass as its own stage. A run checks the free space of the scratch and output volumes before it starts and refuses a RAW run whose developed frames do not fit.
- `focusforge-cli export` writes a linear DNG (`.dng` or `--format dng`) of a camera RAW project, in the camera's own colour space with its colour matrices and as-shot white balance, so a raw converter develops it with exposure, white balance and highlight recovery still open.

### Changed

- Equalize brightness is now off by default, in the GUI and in `focusforge-cli stack`. Turn it on with the switch under PREPARE or with `--set equalize=true`; a project opened from an earlier run keeps the setting it was fused with.
- A project saved by a newer FocusForge is refused on open with a message saying so, instead of being read wrongly.
- Every `focusforge-cli` project command exits with one code per class of error, and the line about a GPU that falls back to the CPU is shorter.
- Each memory cap in Settings is one row with a number and a unit that commits on confirm, and the aperture is called f-stop.
- Dialogs, run failures, brightness notices, slider readouts and the help of the alignment and equalization parameters are worded for photographers; the Levels slider shows plain text values.
- TIFF and JPEG frames are read upright by their orientation tag; an export carries
  Orientation 1, and a carried XMP packet's orientation is rewritten to match.
- A RAW project now keeps highlights above white in its saved result instead of clipping them at 16 bits, so a RAW project saved by an earlier version is refused on open and must be stacked from its source frames again.
- The editor now shows and retouches a RAW project's unclipped result: a live stroke blends on the CPU in 32-bit float, and the canvas, the saved view and every export clip the result once, to 16 bits.
- Camera RAW stacks now fuse and align on unclipped 32-bit float samples and clip once, at the fused result, so highlights above the look's white point no longer clip before blending.
- RAW frames are decoded to the camera's linear colour and read through an invertible
  white-balance, colour-matrix and sRGB-curve transform, and brightness equalization of a
  RAW stack corrects exposure in linear light. A highlight that exceeds white after white
  balance now clips per channel instead of desaturating toward white.

### Fixed

- Opening a folder with BigTIFF or unreadable TIFF frames is refused at once with the reason, instead of failing later in the run.
- Files handed to the program when it is launched, for example with Open with, open in the window.
- The Levels slider stops at the deepest level the stack supports, and a stored deeper value is refused instead of failing the run. A GPU run whose device tile limit is too small falls back to the CPU.
- A dry run of `focusforge-cli` writes no run manifest, and the commands no longer fail when stderr is closed.
- The Diagnostics and Input map tabs show what is in effect.
- Keyboard focus returns to the canvas when the notification center or the calculator closes, and keys the image list does not take reach the window's shortcuts.
- A discarded run no longer leaves its stage view in the Composite pane.
- The macOS app in the DMG is ad-hoc signed, so a downloaded copy no longer opens as
  "damaged"; the getting-started guide explains the Gatekeeper prompt of an app that is
  not notarized.
- The Pause, Resume, Cancel, Retry and Fuse stack buttons draw their icons from the app's
  icon set; on Windows they showed a coloured emoji square and a thin bar.
- The GUI starts on machines without a GPU, such as virtual machines and Remote Desktop
  sessions, by rendering on a software adapter, and on Windows shows an error message when it
  cannot start at all.
- On Windows, `focusforge-cli` and `focusforge-gui` start on a machine without the Visual C++
  redistributable.
- On macOS, a crash report names the crashing program when it was started with a small
  environment, such as from the Finder.
- On Windows, `focusforge-gui --help`, `--version` and argument errors print to the
  terminal that started the program.
- Stacking RAW frames no longer hangs when several threads read tiles while a frame is
  being decoded.
- `focusforge-cli` no longer exits with a panic and a crash report when the program
  reading its output closes the pipe early (for example `focusforge-cli algorithms | head -1`);
  the command finishes with its own exit status.
- On macOS, a short drag on the tab strip or on the comparison window's title bar now
  moves the window.

## [0.1.0] - 2026-10-27

### Added

- Focus stacking for TIFF and supported RAW frame sequences, with alignment,
  three fusion methods, a Z-Map, and CPU or GPU processing.
- A desktop editor and command-line tools for stacking, inspecting, exporting,
  retouching, reviewing, and packaging diagnostics.
- Release packaging recipes for Linux AppImage and Debian packages, an Apple silicon
  macOS DMG, and a Windows MSI.
