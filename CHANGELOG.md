# Changelog

This file records user-visible changes. Its structure follows [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Each stable release now also updates the Arch Linux AUR package
  `focusforge-bin` and submits the new version to winget as
  `FocusForge.FocusForge`. Both are checked in the release workflow before
  anything is submitted.
- Each release carries `cargo-sources.json`, the Rust crate sources of that
  version for an offline Flatpak build.

## [0.1.0] - 2026-10-27

### Added

- Focus stacking for TIFF and supported RAW frame sequences, with alignment,
  three fusion methods, a Z-Map, and CPU or GPU processing.
- A desktop editor and command-line tools for stacking, inspecting, exporting,
  retouching, reviewing, and packaging diagnostics.
- Release packaging recipes for Linux AppImage and Debian packages, an Apple silicon
  macOS DMG, and a Windows MSI.
