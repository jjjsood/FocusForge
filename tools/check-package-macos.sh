#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

[[ $# -eq 1 || $# -eq 2 ]] || { echo "usage: $0 DMG [SYMBOLS]" >&2; exit 2; }
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
dmg=$1
symbols=${2:-"$(dirname "$dmg")/symbols"}
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)
mkdir -p "$root/.scratch"
work=$(mktemp -d "$root/.scratch/check-package-macos.XXXXXX")
mount="$work/mount"
inspect="$work/fixtures"
mkdir "$mount" "$inspect"
attached=0
cleanup() { [[ $attached -eq 0 ]] || hdiutil detach "$mount" -quiet; }
trap cleanup EXIT
hdiutil attach -readonly -nobrowse -mountpoint "$mount" "$dmg" >/dev/null
attached=1
app="$mount/FocusForge.app"
for binary in focusforge-cli focusforge-gui; do
  path="$app/Contents/MacOS/$binary"
  [[ -x $path && $(lipo -archs "$path" | tr ' ' '\n' | sort | tr '\n' ' ') == "arm64 x86_64 " ]] || { echo "invalid universal $binary" >&2; exit 1; }
  file "$path" | grep -Eq 'Mach-O.*(x86_64.*arm64|arm64.*x86_64)' || { echo "not universal Mach-O: $binary" >&2; exit 1; }
  ! otool -l "$path" | grep -Eq '(__DWARF|__debug_)' || { echo "installed binary retains debug sections: $binary" >&2; exit 1; }
  ids() { dwarfdump --uuid "$1" | awk '{print $2 " " $3}' | sort; }
  shipping_ids=$(ids "$path")
  for target in x86_64-apple-darwin aarch64-apple-darwin; do
    debug="$symbols/$target/native/$binary.dSYM"
    [[ -d $debug && $(printf '%s\n' "$shipping_ids" | awk '{print $2}' | tr -d '()' | sort | tr '\n' ' ') == "arm64 x86_64 " && $shipping_ids == $(ids "$debug") ]] || { echo "dSYM UUID mismatch: $binary/$target" >&2; exit 1; }
  done
done
! find "$app" -type d -name '*.dSYM' -print -quit | grep -q .
! find "$app" -type f \( -name '*.debug' -o -name '*.pdb' \) -print -quit | grep -q .
for target in x86_64-apple-darwin aarch64-apple-darwin; do python3 "$root/tools/package-symbols.py" verify "$symbols/$target"; done
plist="$app/Contents/Info.plist"
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$plist") == focusforge-gui ]]
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist") == "$version" ]]
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$plist") == "$version" ]]
[[ $(/usr/libexec/PlistBuddy -c 'Print :CFBundleIconFile' "$plist") == focusforge ]]
identifier=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$plist")
echo "CFBundleIdentifier: $identifier"
[[ $identifier == org.focusforge.FocusForge ]] || { echo "invalid bundle identifier: $identifier" >&2; exit 1; }
for file in LICENSE NOTICE; do cmp "$root/$file" "$app/Contents/Resources/$file"; done
cmp "$root/assets/build/focusforge.icns" "$app/Contents/Resources/focusforge.icns"
python3 "$root/tools/package-cli-assets.py" verify "$root" "$app/Contents/Resources"
[[ $(env -i PATH="$PATH" "$app/Contents/MacOS/focusforge-cli" --version) == "focusforge-cli $version" ]]
env -i PATH="$PATH" "$app/Contents/MacOS/focusforge-cli" --help >/dev/null
cp "$root"/fixtures/mantis-64/frames/*.tif "$inspect/"
env -i PATH="$PATH" "$app/Contents/MacOS/focusforge-cli" info --json "$inspect" | python3 -c 'import json,sys; data=json.load(sys.stdin); files=data["files"]; assert data["frame_count"] == 3 and data["unreadable_count"] == 0 and len(files) == 3; assert all(item["width"] == 64 and item["height"] == 64 and item["samples_per_pixel"] == 3 and item["bits_per_sample"] == [16, 16, 16] for item in files)'
"$root/tools/smoke-release.sh" "$app/Contents/MacOS/focusforge-cli" "$work/smoke"
launch="$work/launch.png"
mkdir -p "$work/gui-data" "$work/gui-config" "$work/gui-cache"
env -i PATH="$PATH" FOCUSFORGE_DATA_DIR="$work/gui-data" FOCUSFORGE_CONFIG_DIR="$work/gui-config" XDG_DATA_HOME="$work/gui-cache" XDG_CONFIG_HOME="$work/gui-config" "$app/Contents/MacOS/focusforge-gui" --exit-after 3000 --screenshot "$launch"
python3 - "$launch" <<'PY'
import struct
import sys
data = open(sys.argv[1], "rb").read(24)
assert data[:8] == b"\x89PNG\r\n\x1a\n" and data[12:16] == b"IHDR"
assert all(value > 0 for value in struct.unpack(">II", data[16:24]))
PY
force_crash() {
  local name=$1 binary=$2
  shift 2
  local crash="$work/crashes/$name" dump metadata status target archive
  mkdir -p "$crash/data" "$crash/config" "$crash/xdg"
  local started crash_pid
  started=$(date +%s)
  set +e
  (ulimit -c 0; exec env -i PATH="$PATH" HOME=/tmp FOCUSFORGE_DATA_DIR="$crash/data" FOCUSFORGE_CONFIG_DIR="$crash/config" XDG_DATA_HOME="$crash/xdg" XDG_CONFIG_HOME="$crash/xdg" FF_FORCE_CRASH=segv "$binary" "$@" >"$crash/stdout" 2>"$crash/stderr") &
  crash_pid=$!
  wait "$crash_pid"; status=$?
  set -e
  [[ $status -ne 0 ]]
  for _ in {1..100}; do
    dump=$(find "$crash/data/focusforge/crashes" -maxdepth 1 -name '*.dmp' -type f -print -quit 2>/dev/null || true)
    metadata=$(find "$crash/data/focusforge/crashes" -maxdepth 1 -name '*.json' -type f -print -quit 2>/dev/null || true)
    [[ -n $dump && -n $metadata ]] && break
    sleep 0.05
  done
  [[ -n ${dump:-} ]] || { echo "missing crash artifact: dump ($name)" >&2; return 1; }
  [[ -n ${metadata:-} ]] || { echo "missing crash artifact: metadata ($name)" >&2; return 1; }
  export CRASH_PID="$crash_pid" CRASH_VERSION="$version" CRASH_STARTED="$started"
  python3 - "$metadata" "$dump" "$name" <<'PY'
import datetime, json, os, pathlib, sys
data = json.load(open(sys.argv[1]))
assert data["binary"] == sys.argv[3]
assert data["minidump"] == pathlib.Path(sys.argv[2]).name
assert data["pid"] == int(os.environ["CRASH_PID"]) and data["version"] == os.environ["CRASH_VERSION"]
stamp = datetime.datetime.fromisoformat(data["timestamp"].replace("Z", "+00:00")).timestamp()
assert stamp >= int(os.environ["CRASH_STARTED"]) - 5
PY
  case "$(uname -m)" in x86_64) target=x86_64-apple-darwin;; arm64) target=aarch64-apple-darwin;; *) exit 1;; esac
  archive=$(find "$symbols/$target" -maxdepth 1 -name 'FocusForge-*-symbols.tar.gz' -type f -print -quit)
  [[ -n $archive ]]
  python3 "$root/tools/symbolicate.py" "$dump" "$archive" > "$crash/stackwalk.json"
}
force_crash focusforge-cli "$app/Contents/MacOS/focusforge-cli" algorithms
force_crash focusforge-gui "$app/Contents/MacOS/focusforge-gui"
