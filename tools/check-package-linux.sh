#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

product_root=${FOCUSFORGE_PRODUCT_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}

validate_assets() {
  local tree=$1 expected=$2
  cmp "$expected/focusforge.desktop" "$tree/share/applications/focusforge.desktop"
  test "$(find "$tree/share/icons/hicolor" -type f | wc -l)" = 10
  diff -qr "$expected/hicolor" "$tree/share/icons/hicolor"
  python3 "$product_root/tools/package-cli-assets.py" verify "$product_root" "$tree"
}

validate_elfs() {
  while IFS= read -r elf; do
    readelf --version-info "$elf" | grep -Eo 'GLIBC_[0-9]+\.[0-9]+' | sort -Vu | awk -F'[_.]' '$2 > 2 || ($2 == 2 && $3 > 35) { exit 1 }'
  done < <(find "$1" -type f -exec sh -c 'file -b "$1" | grep -q ELF && printf "%s\\n" "$1"' sh {} \;)
}

validate_symbols() {
  local tree=$1 symbols=$2 name shipping native identities=()
  python3 /product/tools/package-symbols.py verify "$symbols"
  for name in focusforge-cli focusforge-gui; do
    shipping="$tree/bin/$name"
    native=$(find "$symbols/native" -maxdepth 1 -name "$name.debug" -type f -print -quit)
    test -n "$native"
    readelf -S "$shipping" | grep -F '.gnu_debuglink'
    readelf --string-dump=.gnu_debuglink "$shipping" | grep -F "$(basename "$native")"
    python3 - "$shipping" "$native" <<'PY'
import pathlib, struct, sys, zlib
shipping, native = map(pathlib.Path, sys.argv[1:])
data = shipping.read_bytes()
if data[:6] != b"\x7fELF\x02\x01": raise SystemExit("invalid shipping ELF")
offset = struct.unpack_from("<Q", data, 40)[0]
size, count, strings_index = struct.unpack_from("<HHH", data, 58)
names_offset = struct.unpack_from("<Q", data, offset + strings_index * size + 24)[0]
names_size = struct.unpack_from("<Q", data, offset + strings_index * size + 32)[0]
names = data[names_offset:names_offset + names_size]
for index in range(count):
    entry = offset + index * size
    name, kind = struct.unpack_from("<II", data, entry)
    label = names[name:names.find(b"\0", name)].decode("ascii", "ignore")
    start, length = struct.unpack_from("<QQ", data, entry + 24)
    if label == ".gnu_debuglink" and kind == 1:
        section = data[start:start + length]; basename = section.split(b"\0", 1)[0]
        crc_offset = (len(basename) + 1 + 3) & ~3
        if basename != native.name.encode() or len(section) < crc_offset + 4 or struct.unpack_from("<I", section, crc_offset)[0] != zlib.crc32(native.read_bytes()) & 0xffffffff:
            raise SystemExit("GNU debuglink companion mismatch")
        break
else: raise SystemExit("missing GNU debuglink")
PY
    ! readelf -S "$shipping" | grep -Fq '.debug_info'
    validate_main_address "$name" "$shipping" "$native"
    identities+=(--identity "$name=$shipping,$shipping,$native")
  done
  python3 /product/tools/package-symbols.py verify-linux --root "$symbols" "${identities[@]}"
  ! find "$tree" -type f \( -name '*.debug' -o -name '*.pdb' \) -print -quit | grep -q .
}

validate_main_address() {
  local name=$1 shipping=$2 native=$3 function module shipping_addresses native_addresses address location source relative line
  module=${name#focusforge-}
  function=${name//-/_}'::main'
  mapfile -t shipping_addresses < <(nm -C --defined-only "$shipping" | awk -v wanted="$function" '$NF == wanted { print $1 }')
  mapfile -t native_addresses < <(nm -C --defined-only "$native" | awk -v wanted="$function" '$NF == wanted { print $1 }')
  [ "${#shipping_addresses[@]}" = 1 ]
  [ "${#native_addresses[@]}" = 1 ]
  address=${shipping_addresses[0]}
  [[ $address =~ ^0*[1-9A-Fa-f][0-9A-Fa-f]*$ ]]
  [ "$address" = "${native_addresses[0]}" ]
  mapfile -t location < <(addr2line -f -C -e "$native" "$address")
  [ "${#location[@]}" = 2 ]
  [ "${location[0]}" = "$function" ]
  source=${location[1]}
  [[ $source != *..* ]]
  case $source in
    crates/*) relative=${source#crates/} ;;
    /work/crates/*) relative=${source#/work/crates/} ;;
    *) return 1 ;;
  esac
  [[ $relative != *".."* ]]
  [[ $relative == "$module/src/main.rs:"* ]]
  line=${relative##*:}
  [[ $line =~ ^[1-9][0-9]*$ ]]
  printf 'symbol main: %s address=%s source=crates/%s\n' "$name" "$address" "$relative"
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
image=$(python3 -c 'import sys
section = None
for raw in open(sys.argv[1], encoding="utf-8"):
    line = raw.strip()
    if line == "[linux]": section = "linux"
    elif line.startswith("["): section = None
    elif section and line.startswith("container = "):
        print(line.split(chr(34), 2)[1]); break
else: raise SystemExit("missing Linux container pin")' "$root/packaging/linux-tools.toml")
# Build actual Debian runtime dependencies before artifact checks seal networking.
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)
deb_dependencies=$(dpkg-deb --field "$out/focusforge_${version}_amd64.deb" Depends)
checker_image=$(docker build -q --build-arg DEB_DEPENDENCIES="$deb_dependencies" - <<EOF
FROM $image
ARG DEB_DEPENDENCIES
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq binutils file python3 dpkg && apt-get satisfy -y "\$DEB_DEPENDENCIES"
EOF
)
rm -rf "$work"
mkdir -p "$work/expected/frames" "$work/artifacts" "$work/result"
cp "$root/LICENSE" "$root/NOTICE" "$work/expected/"
cp "$root/assets/focusforge.desktop" "$work/expected/"
cp -a "$root/assets/build/hicolor" "$work/expected/"
printf 'focusforge-cli %s\n' "$version" > "$work/expected/VERSION"
cp "$root/fixtures/mantis-64/frames/00.tif" "$root/fixtures/mantis-64/frames/01.tif" "$root/fixtures/mantis-64/frames/02.tif" "$work/expected/frames/"
(cd "$out" && cp "FocusForge-$version-x86_64.AppImage" "focusforge_${version}_amd64.deb" artifacts.sha256 inventory.txt "$work/artifacts/")
cp -a "$out/symbols" "$work/artifacts/"
mkdir -p "$work/symbolicate"
python3 "$root/tools/package-symbols.py" install stackwalk x86_64-unknown-linux-gnu "$work/symbolicate/symbol-tools" >/dev/null
(cd "$work/expected" && sha256sum LICENSE NOTICE frames/00.tif frames/01.tif frames/02.tif > expected.sha256)
python3 - "$root/packaging/linux-tools.toml" "$work/expected/type2-legal.sha256" <<'PY'
import sys
items = []
current = None
for raw in open(sys.argv[1], encoding="utf-8"):
    line = raw.strip()
    if line == "[[type2_runtime.legal_inputs]]":
        current = {}; items.append(current)
    elif current is not None and "=" in line:
        key, value = (part.strip() for part in line.split("=", 1))
        current[key] = value.strip('"')
with open(sys.argv[2], "w") as output:
    for item in items:
        directory = "sources" if item["name"].endswith(".tar.gz") else "notices"
        output.write(f'{item["sha256"]}  {directory}/{item["name"]}\n')
PY

set +e
docker run --rm --network=none -e FOCUSFORGE_PRODUCT_ROOT=/product -i -v "$work/artifacts:/artifacts:ro" -v "$work/expected:/checks/expected:ro" -v "$work/result:/checks" -v "$work/symbolicate:/symbol-scratch" -v "$root:/product:ro" -v "$root/tools/check-package-linux.sh:/checks/checker.sh:ro" "$checker_image" bash -s -- "$owner_uid" "$owner_gid" <<'APPIMAGE'
set -euo pipefail
owner_uid=$1; owner_gid=$2
trap 'chown -R "$owner_uid:$owner_gid" /checks/appimage /checks/crashes /checks/symbolicate 2>/dev/null || true' EXIT
cd /checks/expected
sha256sum -c expected.sha256
cd /checks
app=$(find /artifacts -maxdepth 1 -name '*.AppImage' -type f -print -quit)
test -n "$app"
(cd /artifacts && sha256sum -c artifacts.sha256)
"$app" --appimage-extract >/dev/null
mv squashfs-root appimage
tree=/checks/appimage/usr
version=$(sed -n 's/^focusforge-cli //p' /checks/expected/VERSION)
source /checks/checker.sh
validate_appimage /checks/appimage /checks/expected /checks/expected/type2-legal.sha256
validate_symbols "$tree" /artifacts/symbols
LD_LIBRARY_PATH="$tree/lib" /product/tools/smoke-release.sh "$tree/bin/focusforge-cli" /checks/appimage/smoke
force_crash() {
  local name=$1 binary=$2
  shift 2
  local crash="/checks/crashes/appimage/$name" dump metadata status
  mkdir -p "$crash/data" "$crash/config" "$crash/xdg"
  local started crash_pid
  started=$(date +%s)
  set +e
  (ulimit -c 0; exec env -i PATH=/usr/bin:/bin HOME=/tmp LD_LIBRARY_PATH="${LD_LIBRARY_PATH:-}" FOCUSFORGE_DATA_DIR="$crash/data" FOCUSFORGE_CONFIG_DIR="$crash/config" XDG_DATA_HOME="$crash/xdg" XDG_CONFIG_HOME="$crash/xdg" FF_FORCE_CRASH=segv "$binary" "$@" >"$crash/stdout" 2>"$crash/stderr") &
  crash_pid=$!
  wait "$crash_pid"; status=$?
  set -e
  test "$status" -ne 0
  for _ in $(seq 1 100); do
    dump=$(find "$crash/data/focusforge/crashes" -maxdepth 1 -name '*.dmp' -type f -print -quit 2>/dev/null || true)
    metadata=$(find "$crash/data/focusforge/crashes" -maxdepth 1 -name '*.json' -type f -print -quit 2>/dev/null || true)
    test -n "$dump" && test -n "$metadata" && break
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
  archive=$(find /artifacts/symbols -maxdepth 1 -name 'FocusForge-*-x86_64-unknown-linux-gnu-symbols.tar.gz' -type f -print -quit)
  test -n "$archive"
  FOCUSFORGE_SYMBOL_SCRATCH=/symbol-scratch python3 /product/tools/symbolicate.py "$dump" "$archive" > "$crash/stackwalk.json"
}
LD_LIBRARY_PATH="$tree/lib" force_crash focusforge-cli "$tree/bin/focusforge-cli" algorithms
LD_LIBRARY_PATH="$tree/lib" force_crash focusforge-gui "$tree/bin/focusforge-gui"
APPIMAGE
status=$?
set -e
[ "$status" = 0 ] || exit "$status"

set +e
docker run --rm --network=none -e FOCUSFORGE_PRODUCT_ROOT=/product -i -v "$work/artifacts:/artifacts:ro" -v "$work/expected:/checks/expected:ro" -v "$work/result:/checks" -v "$work/symbolicate:/symbol-scratch" -v "$root:/product:ro" -v "$root/tools/check-package-linux.sh:/checks/checker.sh:ro" "$checker_image" bash -s -- "$owner_uid" "$owner_gid" <<'DEB'
set -euo pipefail
owner_uid=$1; owner_gid=$2
trap 'chown -R "$owner_uid:$owner_gid" /checks/crashes /checks/symbolicate /checks/deb-info.txt /checks/deb-info.json /checks/deb-smoke 2>/dev/null || true' EXIT
deb=$(find /artifacts -maxdepth 1 -name '*.deb' -type f -print -quit)
test -n "$deb"
version=$(dpkg-deb --field "$deb" Version)
dpkg-deb --field "$deb" Depends | grep -q .
# The minimal base image excludes documentation; retain the package legal payload.
apt-get -o 'DPkg::Options::=--path-include=/usr/share/doc/focusforge/*' -o 'DPkg::Options::=--path-include=/usr/share/man/man1/focusforge-cli*.gz' install -y -qq "$deb"
tree=/usr
test -x "$tree/bin/focusforge-cli"; test -x "$tree/bin/focusforge-gui"
test -f "$tree/share/applications/focusforge.desktop"
test -f "$tree/share/doc/focusforge/LICENSE"; test -f "$tree/share/doc/focusforge/NOTICE"
test -f "$tree/share/doc/focusforge/native-notices.txt"
source /checks/checker.sh
validate_assets "$tree" /checks/expected
validate_elfs "$tree/bin"
validate_symbols "$tree" /artifacts/symbols
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
/product/tools/smoke-release.sh "/usr/bin/focusforge-cli" /checks/deb-smoke
env -i PATH=/usr/bin:/bin HOME=/tmp "$tree/bin/focusforge-gui" --help >/dev/null
force_crash() {
  local name=$1 binary=$2
  shift 2
  local crash="/checks/crashes/debian/$name" dump metadata status
  mkdir -p "$crash/data" "$crash/config" "$crash/xdg"
  local started crash_pid
  started=$(date +%s)
  set +e
  (ulimit -c 0; exec env -i PATH=/usr/bin:/bin HOME=/tmp FOCUSFORGE_DATA_DIR="$crash/data" FOCUSFORGE_CONFIG_DIR="$crash/config" XDG_DATA_HOME="$crash/xdg" XDG_CONFIG_HOME="$crash/xdg" FF_FORCE_CRASH=segv "$binary" "$@" >"$crash/stdout" 2>"$crash/stderr") &
  crash_pid=$!
  wait "$crash_pid"; status=$?
  set -e
  test "$status" -ne 0
  for _ in $(seq 1 100); do
    dump=$(find "$crash/data/focusforge/crashes" -maxdepth 1 -name '*.dmp' -type f -print -quit 2>/dev/null || true)
    metadata=$(find "$crash/data/focusforge/crashes" -maxdepth 1 -name '*.json' -type f -print -quit 2>/dev/null || true)
    test -n "$dump" && test -n "$metadata" && break
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
  archive=$(find /artifacts/symbols -maxdepth 1 -name 'FocusForge-*-x86_64-unknown-linux-gnu-symbols.tar.gz' -type f -print -quit)
  test -n "$archive"
  FOCUSFORGE_SYMBOL_SCRATCH=/symbol-scratch python3 /product/tools/symbolicate.py "$dump" "$archive" > "$crash/stackwalk.json"
}
force_crash focusforge-cli "$tree/bin/focusforge-cli" algorithms
force_crash focusforge-gui "$tree/bin/focusforge-gui"
DEB
status=$?
set -e
[ "$status" = 0 ] || exit "$status"
echo "package artifacts valid: $out"
fi
