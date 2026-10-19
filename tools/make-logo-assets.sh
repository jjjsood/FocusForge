#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/make-logo-assets.sh
#
# Rasterises the brand SVGs in assets/ into everything the three target
# platforms and the repository page need. Output goes to assets/build/, which
# is generated in full on every run and should never be edited by hand.
#
# Two families come out of this. assets/logo.svg is the bare mark, transparent,
# for documents and pages that bring their own background. assets/app-icon.svg
# is the same mark on a dark rounded ground, and that is what the desktop gets:
# a transparent dark mark disappears on a dark taskbar, leaving the hammer
# floating on its own.
#
# Two sources per family, not one. The masters are drawn on a 128 grid with
# 6 unit plates and 6 unit gaps; below 48 px those land on fractions of a pixel
# and the anvil's planes blur into one block. assets/logo-small.svg is the same
# mark redrawn on a 32 grid for exactly that range. This script picks the
# source per size -- small cut at 32 px and under, master from 48 px up -- so
# the icon stays legible the whole way down instead of being one drawing
# scaled past its limit.
#
# The .icns container is written by tools/lib/write-icns.py because the
# ImageMagick build here has no ICNS coder. The .ico is ImageMagick's own.
#
# The wordmark lockups are *not* rebuilt here: they need Inter installed and
# they are checked in with their text already converted to paths. Rebuild them
# with tools/lib/make-wordmark.py when the type changes.
#
# Exit: 0 pass or skip, 1 fail.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

if [ ! -f assets/logo.svg ]; then
  echo "SKIP: no assets/logo.svg yet"
  exit 0
fi

for tool in inkscape magick python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "FAIL: $tool is required to build the logo assets." >&2
    exit 1
  fi
done

OUT=assets/build
rm -rf "$OUT"
mkdir -p "$OUT/png" "$OUT/appicon" "$OUT/hicolor/scalable/apps"

# Which drawing serves a given pixel size.
source_for() {
  if [ "$1" -le 32 ]; then echo assets/logo-small.svg; else echo assets/logo.svg; fi
}

icon_source_for() {
  if [ "$1" -le 32 ]; then echo assets/app-icon-small.svg; else echo assets/app-icon.svg; fi
}

# render SRC SIZE DEST -- transparent, no metadata, so two runs are identical.
render() {
  inkscape "$1" -o "$3" -w "$2" -h "$2" --export-background-opacity=0 >/dev/null 2>&1
  magick "$3" -strip -define png:exclude-chunk=time,date "$3"
}

echo "logo assets -> $OUT"

# --- the plain PNG ladder -----------------------------------------------------
LADDER="16 24 32 48 64 128 256 512 1024"
for size in $LADDER; do
  render "$(source_for "$size")" "$size" "$OUT/png/focusforge-$size.png"
done

# The inverse cut only serves dark UI chrome and dark documentation pages, so
# it needs display sizes, not the icon ladder.
for size in 128 256 512 1024; do
  src=assets/logo-inverse.svg
  [ "$size" -le 32 ] && src=assets/logo-small-inverse.svg
  render "$src" "$size" "$OUT/png/focusforge-inverse-$size.png"
done

# --- the desktop icon, on its ground ------------------------------------------
for size in $LADDER; do
  render "$(icon_source_for "$size")" "$size" "$OUT/appicon/focusforge-$size.png"
done

# --- Linux: the hicolor theme tree -------------------------------------------
for size in 16 22 24 32 48 64 128 256 512; do
  dir="$OUT/hicolor/${size}x${size}/apps"
  mkdir -p "$dir"
  render "$(icon_source_for "$size")" "$size" "$dir/focusforge.png"
done
cp assets/app-icon.svg "$OUT/hicolor/scalable/apps/focusforge.svg"

# --- Windows: one .ico holding every size Explorer asks for ------------------
magick \
  "$OUT/appicon/focusforge-16.png" "$OUT/appicon/focusforge-24.png" \
  "$OUT/appicon/focusforge-32.png" "$OUT/appicon/focusforge-48.png" \
  "$OUT/appicon/focusforge-64.png" "$OUT/appicon/focusforge-128.png" \
  "$OUT/appicon/focusforge-256.png" \
  -strip "$OUT/focusforge.ico"

# --- macOS: the icon suite ----------------------------------------------------
# Retina types repeat a pixel size on purpose: ic11 is 16 points at 2x, ic12 is
# 32 points at 2x, and so on. The same file serves both readings.
python3 tools/lib/write-icns.py "$OUT/focusforge.icns" \
  "icp4=$OUT/appicon/focusforge-16.png" \
  "icp5=$OUT/appicon/focusforge-32.png" \
  "icp6=$OUT/appicon/focusforge-64.png" \
  "ic07=$OUT/appicon/focusforge-128.png" \
  "ic08=$OUT/appicon/focusforge-256.png" \
  "ic09=$OUT/appicon/focusforge-512.png" \
  "ic10=$OUT/appicon/focusforge-1024.png" \
  "ic11=$OUT/appicon/focusforge-32.png" \
  "ic12=$OUT/appicon/focusforge-64.png" \
  "ic13=$OUT/appicon/focusforge-256.png" \
  "ic14=$OUT/appicon/focusforge-512.png"

# --- documentation and repository page ---------------------------------------
inkscape assets/wordmark-horizontal.svg -o "$OUT/wordmark-horizontal.png" \
  -w 880 --export-background-opacity=0 >/dev/null 2>&1
inkscape assets/wordmark-horizontal-inverse.svg -o "$OUT/wordmark-horizontal-inverse.png" \
  -w 880 --export-background-opacity=0 >/dev/null 2>&1
magick "$OUT/wordmark-horizontal.png" -strip "$OUT/wordmark-horizontal.png"
magick "$OUT/wordmark-horizontal-inverse.png" -strip "$OUT/wordmark-horizontal-inverse.png"

# GitHub renders the social preview at 1280x640 and crops nothing.
inkscape assets/wordmark-horizontal.svg -o "$OUT/_social.png" \
  -w 820 --export-background-opacity=0 >/dev/null 2>&1
inkscape assets/wordmark-horizontal-inverse.svg -o "$OUT/_social-dark.png" \
  -w 820 --export-background-opacity=0 >/dev/null 2>&1
magick -size 1280x640 canvas:'#FFFFFF' "$OUT/_social.png" \
  -gravity center -composite -strip "$OUT/social-preview.png"
magick -size 1280x640 canvas:'#23303B' "$OUT/_social-dark.png" \
  -gravity center -composite -strip "$OUT/social-preview-dark.png"
rm -f "$OUT/_social.png" "$OUT/_social-dark.png"

count=$(find "$OUT" -type f | wc -l)
echo "PASS: $count files written under $OUT"
