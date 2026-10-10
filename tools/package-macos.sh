#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
output="$root/target/distrib/native/macos"
if [[ ${1:-} == --output ]]; then output=$2; shift 2; fi
[[ $# -eq 0 ]] || { echo "usage: $0 [--output DIR]" >&2; exit 2; }
output=$(python3 -c 'from pathlib import Path; import sys; print(Path(sys.argv[1]).resolve(strict=False))' "$output")
case "$output" in "$root"/target/distrib/native/*) ;; *) echo "output must stay below target/distrib/native" >&2; exit 2;; esac
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)
[[ -n $version ]] || { echo "missing workspace version" >&2; exit 1; }
for command in cargo lipo hdiutil plutil shasum; do command -v "$command" >/dev/null || { echo "missing $command" >&2; exit 1; }; done
for target in x86_64-apple-darwin aarch64-apple-darwin; do
  cargo build --locked --release --target "$target" -p focusforge-cli --bin focusforge-cli
  cargo build --locked --release --target "$target" -p focusforge-gui --bin focusforge-gui
done
mkdir -p "$root/.scratch"
staging=$(mktemp -d "$root/.scratch/package-macos.XXXXXX")
trap 'rm -rf "$staging"' EXIT
app="$staging/FocusForge.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
for binary in focusforge-cli focusforge-gui; do
  x86="$root/target/x86_64-apple-darwin/release/$binary"
  arm="$root/target/aarch64-apple-darwin/release/$binary"
  lipo -create "$x86" "$arm" -output "$app/Contents/MacOS/$binary"
  [[ $(lipo -archs "$app/Contents/MacOS/$binary" | tr ' ' '\n' | sort | tr '\n' ' ') == "arm64 x86_64 " ]] || { echo "unexpected universal slices for $binary" >&2; exit 1; }
done
sed "s/__VERSION__/$version/g" "$root/packaging/macos/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
cp "$root/assets/build/focusforge.icns" "$app/Contents/Resources/focusforge.icns"
cp "$root/LICENSE" "$root/NOTICE" "$app/Contents/Resources/"
mkdir -p "$output"
candidate="$staging/FocusForge-$version-universal.dmg"
hdiutil create -quiet -volname FocusForge -srcfolder "$app" -ov -format UDZO "$candidate"
hash=$(shasum -a 256 "$candidate" | awk '{print $1}')
revision=$(git -C "$root" rev-parse HEAD)
printf '{"kind":"dmg","version":"%s","revision":"%s","artifact":"FocusForge-%s-universal.dmg","sha256":"%s"}\n' "$version" "$revision" "$version" "$hash" > "$staging/native-receipt.json"
replacement="$output.next"
previous="$output.previous"
rm -rf "$replacement" "$previous"
mkdir -p "$replacement"
mv "$candidate" "$replacement/FocusForge-$version-universal.dmg"
mv "$staging/native-receipt.json" "$replacement/native-receipt.json"
if [[ -e $output ]]; then mv "$output" "$previous"; fi
if ! mv "$replacement" "$output"; then
  [[ ! -e $previous ]] || mv "$previous" "$output"
  exit 1
fi
rm -rf "$previous"
