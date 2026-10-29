# Changelog

This file records user-visible changes. Its structure follows [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

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
