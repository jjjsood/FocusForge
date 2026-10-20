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
for command in cargo lipo hdiutil plutil shasum dsymutil strip dwarfdump; do command -v "$command" >/dev/null || { echo "missing $command" >&2; exit 1; }; done
# dsymutil below reads the debug map of the linked binary, which points at the object files; with the
# profile's "off" rustc deletes those after linking and the dSYM loses the binaries' own line tables.
export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=unpacked
target=aarch64-apple-darwin
cargo build --locked --release --target "$target" -p focusforge-cli --bin focusforge-cli
cargo build --locked --release --target "$target" -p focusforge-gui --bin focusforge-gui
mkdir -p "$root/.scratch"
staging=$(mktemp -d "$root/.scratch/package-macos.XXXXXX")
trap 'rm -rf "$staging"' EXIT
app="$staging/FocusForge.app"
dump_syms=$(python3 "$root/tools/package-symbols.py" install dump_syms "$target" "$staging/tools")
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
mkdir -p "$staging/original" "$staging/native"
for binary in focusforge-cli focusforge-gui; do
  cp "$root/target/$target/release/$binary" "$staging/original/$binary"
  dsymutil "$staging/original/$binary" -o "$staging/native/$binary.dSYM"
  strip -S -o "$app/Contents/MacOS/$binary" "$staging/original/$binary"
  [[ $(lipo -archs "$app/Contents/MacOS/$binary") == arm64 ]] || { echo "unexpected architecture for $binary" >&2; exit 1; }
  ids() { dwarfdump --uuid "$1" | awk '{print $2 " " $3}' | sort; }
  original_ids=$(ids "$staging/original/$binary")
  [[ $(printf '%s\n' "$original_ids" | awk '{print $2}' | tr -d '()' | sort | tr '\n' ' ') == "arm64 " && $original_ids == $(ids "$app/Contents/MacOS/$binary") && $original_ids == $(ids "$staging/native/$binary.dSYM") ]] || { echo "Mach-O and dSYM UUID mismatch for $binary" >&2; exit 1; }
done
python3 "$root/tools/package-symbols.py" build --platform "$target" --version "$version" --dump-syms "$dump_syms" --output "$staging/symbols/$target" --output-root "$staging" --symbols-per-native 1 \
  --native focusforge-cli="$staging/native/focusforge-cli.dSYM" --native focusforge-gui="$staging/native/focusforge-gui.dSYM" \
  --binary focusforge-cli="$staging/original/focusforge-cli" --binary focusforge-gui="$staging/original/focusforge-gui"
sed "s/__VERSION__/$version/g" "$root/packaging/macos/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
cp "$root/assets/build/focusforge.icns" "$app/Contents/Resources/focusforge.icns"
cp "$root/LICENSE" "$root/NOTICE" "$app/Contents/Resources/"
python3 "$root/tools/package-cli-assets.py" stage "$root" "$app/Contents/Resources"
mkdir -p "$output"
candidate="$staging/FocusForge-$version-aarch64.dmg"
hdiutil create -quiet -volname FocusForge -srcfolder "$app" -ov -format UDZO "$candidate"
hash=$(shasum -a 256 "$candidate" | awk '{print $1}')
revision=$(git -C "$root" rev-parse HEAD)
printf '{"kind":"dmg","version":"%s","revision":"%s","artifact":"FocusForge-%s-aarch64.dmg","sha256":"%s"}\n' "$version" "$revision" "$version" "$hash" > "$staging/native-receipt.json"
replacement="$output.next"
previous="$output.previous"
rm -rf "$replacement" "$previous"
mkdir -p "$replacement"
mv "$candidate" "$replacement/FocusForge-$version-aarch64.dmg"
mv "$staging/native-receipt.json" "$replacement/native-receipt.json"
mv "$staging/symbols" "$replacement/symbols"
if [[ -e $output ]]; then mv "$output" "$previous"; fi
if ! mv "$replacement" "$output"; then
  [[ ! -e $previous ]] || mv "$previous" "$output"
  exit 1
fi
rm -rf "$previous"
