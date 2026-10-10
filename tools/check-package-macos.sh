#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

[[ $# -eq 1 ]] || { echo "usage: $0 DMG" >&2; exit 2; }
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
dmg=$1
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)
mkdir -p "$root/.scratch"
work=$(mktemp -d "$root/.scratch/check-package-macos.XXXXXX")
mount="$work/mount"
inspect="$work/fixtures"
mkdir "$mount" "$inspect"
attached=0
cleanup() { [[ $attached -eq 0 ]] || hdiutil detach "$mount" -quiet; rm -rf "$work"; }
trap cleanup EXIT
hdiutil attach -readonly -nobrowse -mountpoint "$mount" "$dmg" >/dev/null
attached=1
app="$mount/FocusForge.app"
for binary in focusforge-cli focusforge-gui; do
  path="$app/Contents/MacOS/$binary"
  [[ -x $path && $(lipo -archs "$path" | tr ' ' '\n' | sort | tr '\n' ' ') == "arm64 x86_64 " ]] || { echo "invalid universal $binary" >&2; exit 1; }
  file "$path" | grep -Eq 'Mach-O.*(x86_64.*arm64|arm64.*x86_64)' || { echo "not universal Mach-O: $binary" >&2; exit 1; }
done
plist="$app/Contents/Info.plist"
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$plist") == focusforge-gui ]]
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist") == "$version" ]]
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$plist") == "$version" ]]
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconFile' "$plist") == focusforge ]]
for file in LICENSE NOTICE; do cmp "$root/$file" "$app/Contents/Resources/$file"; done
cmp "$root/assets/build/focusforge.icns" "$app/Contents/Resources/focusforge.icns"
[[ $(env -i PATH="$PATH" "$app/Contents/MacOS/focusforge-cli" --version) == "focusforge-cli $version" ]]
env -i PATH="$PATH" "$app/Contents/MacOS/focusforge-cli" --help >/dev/null
cp "$root"/fixtures/mantis-64/frames/*.tif "$inspect/"
env -i PATH="$PATH" "$app/Contents/MacOS/focusforge-cli" info --json "$inspect" | python3 -c 'import json,sys; data=json.load(sys.stdin); files=data["files"]; assert data["frame_count"] == 3 and data["unreadable_count"] == 0 and len(files) == 3; assert all(item["width"] == 64 and item["height"] == 64 and item["samples_per_pixel"] == 3 and item["bits_per_sample"] == [16, 16, 16] for item in files)'
