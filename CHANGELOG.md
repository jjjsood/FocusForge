# Changelog

This file records user-visible changes. Its structure follows [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Frame sets of 8-bit TIFF files open and stack; their samples are widened to the 16-bit
  range.
- JPEG frames (`.jpg`, `.jpeg`) open, import and stack in `focusforge-cli` and the GUI. Their
  EXIF capture time groups a folder, and an export from JPEG frames carries their EXIF.
- `focusforge-cli export` writes a linear DNG (`.dng` or `--format dng`) of a camera RAW project, in the camera's own colour space with its colour matrices and as-shot white balance, so a raw converter develops it with exposure, white balance and highlight recovery still open.

### Changed

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
