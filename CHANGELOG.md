# Changelog

This file records user-visible changes. Its structure follows [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `focusforge-cli export` writes a linear DNG (`.dng` or `--format dng`) of a camera RAW project, in the camera's own colour space with its colour matrices and as-shot white balance, so a raw converter develops it with exposure, white balance and highlight recovery still open.

### Changed

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
- On Windows, `focusforge-gui --help`, `--version` and argument errors print to the
  terminal that started the program.
- Stacking RAW frames no longer hangs when several threads read tiles while a frame is
  being decoded.
- `focusforge-cli` no longer exits with a panic and a crash report when the program
  reading its output closes the pipe early (for example `focusforge-cli algorithms | head -1`);
  the command finishes with its own exit status.

## [0.1.0] - 2026-10-27

### Added

- Focus stacking for TIFF and supported RAW frame sequences, with alignment,
  three fusion methods, a Z-Map, and CPU or GPU processing.
- A desktop editor and command-line tools for stacking, inspecting, exporting,
  retouching, reviewing, and packaging diagnostics.
- Release packaging recipes for Linux AppImage and Debian packages, an Apple silicon
  macOS DMG, and a Windows MSI.
