#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

validate_assets() {
  local tree=$1 expected=$2
  cmp "$expected/focusforge.desktop" "$tree/share/applications/focusforge.desktop"
  test "$(find "$tree/share/icons/hicolor" -type f | wc -l)" = 10
  diff -qr "$expected/hicolor" "$tree/share/icons/hicolor"
}

validate_elfs() {
  while IFS= read -r elf; do
    readelf --version-info "$elf" | grep -Eo 'GLIBC_[0-9]+\.[0-9]+' | sort -Vu | awk -F'[_.]' '$2 > 2 || ($2 == 2 && $3 > 35) { exit 1 }'
  done < <(find "$1" -type f -exec sh -c 'file -b "$1" | grep -q ELF && printf "%s\\n" "$1"' sh {} \;)
}

validate_cli() {
  local cli=$1 expected=$2
  env -i PATH=/usr/bin:/bin HOME=/tmp "$cli" --help >/dev/null
  test "$(env -i PATH=/usr/bin:/bin HOME=/tmp "$cli" --version)" = "$(cat "$expected/VERSION")"
}

validate_appimage_desktop() {
  local root=$1 expected=$2 version
  if [ -L "$root/focusforge.desktop" ]; then
    test "$(readlink "$root/focusforge.desktop")" = usr/share/applications/focusforge.desktop
    test -e "$root/$(readlink "$root/focusforge.desktop")"
    return
  fi
  version=$(sed -n 's/^focusforge-cli //p' "$expected/VERSION")
  test -f "$root/focusforge.desktop"
  { cat "$expected/focusforge.desktop"; printf 'X-AppImage-Version=%s\n' "$version"; } | cmp - "$root/focusforge.desktop"
}

validate_appimage() {
  local appimage_root=$1 expected=$2 legal=$3 tree
  tree="$appimage_root/usr"
  test -x "$tree/bin/focusforge-cli"; test -x "$tree/bin/focusforge-gui"
  test -f "$tree/share/applications/focusforge.desktop"
  test -f "$tree/share/doc/focusforge/LICENSE"; test -f "$tree/share/doc/focusforge/NOTICE"
  test -f "$tree/share/doc/focusforge/native-notices.txt"
  validate_assets "$tree" "$expected"
  validate_appimage_desktop "$appimage_root" "$expected"
  test -L "$appimage_root/focusforge.png"
  test "$(readlink "$appimage_root/focusforge.png")" = usr/share/icons/hicolor/256x256/apps/focusforge.png
  test -e "$appimage_root/$(readlink "$appimage_root/focusforge.png")"
  find "$tree/lib" -type f -name 'libfontconfig.so.*' -print -quit | grep -q .
  grep -F 'libfontconfig' "$tree/share/doc/focusforge/native-notices.txt"
  cmp "$expected/LICENSE" "$tree/share/doc/focusforge/LICENSE"
  cmp "$expected/NOTICE" "$tree/share/doc/focusforge/NOTICE"
  (cd "$tree/share/doc/focusforge/type2-runtime" && sha256sum -c "$legal")
  grep -F 'dd6cebedcbddde9c82f89b011e8e1d40b6e43868' "$tree/share/doc/focusforge/type2-runtime/RELINKING.md"
  grep -F 'libfuse 3.15.0' "$tree/share/doc/focusforge/type2-runtime/RELINKING.md"
  while IFS= read -r reference; do
    test -f "$tree/share/doc/focusforge/native-licenses/$(basename "$reference")"
  done < <(grep -hEo '/usr/share/common-licenses/[A-Za-z0-9.+-]+' "$tree/share/doc/focusforge/native-licenses"/*.copyright | sed 's/\.$//' | sort -u)
  validate_elfs "$appimage_root"
  env -i PATH=/usr/bin:/bin HOME=/tmp "$appimage_root/AppRun" --help >/dev/null
  validate_cli "$tree/bin/focusforge-cli" "$expected"
  env -i PATH=/usr/bin:/bin HOME=/tmp "$tree/bin/focusforge-cli" info "$expected/frames" | tee "$appimage_root/info.txt"
  for frame in 00.tif 01.tif 02.tif; do grep -F "$expected/frames/$frame" "$appimage_root/info.txt"; done
  test "$(grep -Fc '64x64 3ch 16-bit' "$appimage_root/info.txt")" = 3
  env -i PATH=/usr/bin:/bin HOME=/tmp "$tree/bin/focusforge-cli" info --json "$expected/frames" > "$appimage_root/info.json"
  python3 - "$appimage_root/info.json" <<'PY'
import json
import sys
report = json.load(open(sys.argv[1]))
files = report.get("files")
assert report.get("frame_count") == 3 and report.get("unreadable_count") == 0
assert isinstance(files, list) and len(files) == 3
assert [row.get("name") for row in files] == ["00.tif", "01.tif", "02.tif"]
for row in files:
    assert (row.get("width"), row.get("height"), row.get("samples_per_pixel"), row.get("bits_per_sample")) == (64, 64, 3, [16, 16, 16])
PY
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
if [ "${1:-}" = --validate-appimage ]; then
  [ "$#" = 4 ] || { echo 'usage: check-package-linux.sh --validate-appimage ROOT EXPECTED LEGAL' >&2; exit 2; }
  validate_appimage "$2" "$3" "$4"
  exit $?
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=$(cd -- "${1:-"$root/.scratch/package-linux"}" && pwd)
work="$out/check"
owner_uid=$(id -u)
owner_gid=$(id -g)
image=$(python3 - "$root/packaging/linux-tools.toml" <<'PY'
import sys, tomllib
print(tomllib.load(open(sys.argv[1], "rb"))["linux"]["container"])
PY
)
rm -rf "$work"
mkdir -p "$work/expected/frames" "$work/artifacts" "$work/result"
cp "$root/LICENSE" "$root/NOTICE" "$work/expected/"
cp "$root/assets/focusforge.desktop" "$work/expected/"
cp -a "$root/assets/build/hicolor" "$work/expected/"
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)
printf 'focusforge-cli %s\n' "$version" > "$work/expected/VERSION"
cp "$root/fixtures/mantis-64/frames/00.tif" "$root/fixtures/mantis-64/frames/01.tif" "$root/fixtures/mantis-64/frames/02.tif" "$work/expected/frames/"
(cd "$out" && cp "FocusForge-$version-x86_64.AppImage" "focusforge_${version}_amd64.deb" artifacts.sha256 inventory.txt "$work/artifacts/")
(cd "$work/expected" && sha256sum LICENSE NOTICE frames/00.tif frames/01.tif frames/02.tif > expected.sha256)
python3 - "$root/packaging/linux-tools.toml" "$work/expected/type2-legal.sha256" <<'PY'
import sys
import tomllib
pins = tomllib.load(open(sys.argv[1], "rb"))
with open(sys.argv[2], "w") as output:
    for item in pins["type2_runtime"]["legal_inputs"]:
        directory = "sources" if item["name"].endswith(".tar.gz") else "notices"
        output.write(f'{item["sha256"]}  {directory}/{item["name"]}\n')
PY

set +e
docker run --rm -i -v "$work/artifacts:/artifacts:ro" -v "$work/expected:/checks/expected:ro" -v "$work/result:/checks" -v "$root/tools/check-package-linux.sh:/checks/checker.sh:ro" "$image" bash -s -- "$owner_uid" "$owner_gid" <<'APPIMAGE'
set -euo pipefail
owner_uid=$1; owner_gid=$2
trap 'chown -R "$owner_uid:$owner_gid" /checks/appimage 2>/dev/null || true' EXIT
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq binutils file python3
cd /checks/expected
sha256sum -c expected.sha256
cd /checks
app=$(find /artifacts -maxdepth 1 -name '*.AppImage' -type f -print -quit)
test -n "$app"
(cd /artifacts && sha256sum -c artifacts.sha256)
"$app" --appimage-extract >/dev/null
mv squashfs-root appimage
tree=/checks/appimage/usr
source /checks/checker.sh
validate_appimage /checks/appimage /checks/expected /checks/expected/type2-legal.sha256
APPIMAGE
status=$?
set -e
[ "$status" = 0 ] || exit "$status"

set +e
docker run --rm -i -v "$work/artifacts:/artifacts:ro" -v "$work/expected:/checks/expected:ro" -v "$work/result:/checks" -v "$root/tools/check-package-linux.sh:/checks/checker.sh:ro" "$image" bash -s -- "$owner_uid" "$owner_gid" <<'DEB'
set -euo pipefail
owner_uid=$1; owner_gid=$2
trap 'chown "$owner_uid:$owner_gid" /checks/deb-info.txt /checks/deb-info.json 2>/dev/null || true' EXIT
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq dpkg binutils file python3
deb=$(find /artifacts -maxdepth 1 -name '*.deb' -type f -print -quit)
test -n "$deb"
dpkg-deb --field "$deb" Depends | grep -q .
# The minimal base image excludes documentation; retain the package legal payload.
apt-get -o 'DPkg::Options::=--path-include=/usr/share/doc/focusforge/*' install -y -qq "$deb"
tree=/usr
test -x "$tree/bin/focusforge-cli"; test -x "$tree/bin/focusforge-gui"
test -f "$tree/share/applications/focusforge.desktop"
test -f "$tree/share/doc/focusforge/LICENSE"; test -f "$tree/share/doc/focusforge/NOTICE"
test -f "$tree/share/doc/focusforge/native-notices.txt"
source /checks/checker.sh
validate_assets "$tree" /checks/expected
validate_elfs "$tree/bin"
cmp /checks/expected/LICENSE "$tree/share/doc/focusforge/LICENSE"
cmp /checks/expected/NOTICE "$tree/share/doc/focusforge/NOTICE"
dpkg-deb --field "$deb" License | grep -Fx 'GPL-3.0-only'
validate_cli "$tree/bin/focusforge-cli" /checks/expected
env -i PATH=/usr/bin:/bin HOME=/tmp "$tree/bin/focusforge-cli" info /checks/expected/frames | tee /checks/deb-info.txt
for frame in 00.tif 01.tif 02.tif; do grep -F "/checks/expected/frames/$frame" /checks/deb-info.txt; done
test "$(grep -Fc '64x64 3ch 16-bit' /checks/deb-info.txt)" = 3
env -i PATH=/usr/bin:/bin HOME=/tmp "$tree/bin/focusforge-cli" info --json /checks/expected/frames > /checks/deb-info.json
python3 - /checks/deb-info.json <<'PY'
import json
import sys
report = json.load(open(sys.argv[1]))
files = report.get("files")
assert report.get("frame_count") == 3 and report.get("unreadable_count") == 0
assert isinstance(files, list) and len(files) == 3
assert [row.get("name") for row in files] == ["00.tif", "01.tif", "02.tif"]
for row in files:
    assert (row.get("width"), row.get("height"), row.get("samples_per_pixel"), row.get("bits_per_sample")) == (64, 64, 3, [16, 16, 16])
PY
env -i PATH=/usr/bin:/bin HOME=/tmp "$tree/bin/focusforge-gui" --help >/dev/null
DEB
status=$?
set -e
[ "$status" = 0 ] || exit "$status"
echo "package artifacts valid: $out"
fi
