#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

if [ "${1:-}" = --verify-sha256 ]; then
  [ "$#" = 3 ] || { echo 'usage: package-linux.sh --verify-sha256 FILE SHA256' >&2; exit 2; }
  printf '%s  %s\n' "$3" "$2" | sha256sum -c -
  exit 0
fi

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out="$root/.scratch/package-linux"
pins="$root/packaging/linux-tools.toml"

write_legal_inputs() {
  local source=$1 target=$2 temporary
  mkdir -p "$(dirname "$target")"
  temporary=$(mktemp "${target}.XXXXXX")
  if ! python3 - "$source" > "$temporary" <<'PY'
import re
import sys

def parse(text):
    out, section, item = {}, None, None
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        array_header = re.fullmatch(r"\[\[\s*([A-Za-z0-9_.-]+)\s*\]\]\s*(?:#.*)?", line)
        table_header = re.fullmatch(r"\[\s*([A-Za-z0-9_.-]+)\s*\]\s*(?:#.*)?", line)
        if array_header:
            section = array_header.group(1)
            item = {}
            out.setdefault(section, []).append(item)
        elif table_header:
            section = table_header.group(1)
            item = out.setdefault(section, {})
        elif line.startswith("["):
            raise SystemExit("unsupported pin table header")
        elif section and "=" in line:
            key, value = (part.strip() for part in line.split("=", 1))
            if value.startswith('"') and value.endswith('"'):
                value = value[1:-1]
            item[key] = value
    return out

pins = parse(open(sys.argv[1], encoding="utf-8").read())
sha256 = re.compile(r"[0-9a-f]{64}")
if not re.fullmatch(r"ubuntu@sha256:[0-9a-f]{64}", pins["linux"]["container"]):
    raise SystemExit("invalid Linux container pin")
for section in ("appimagetool", "type2_runtime"):
    value = pins[section]
    if not isinstance(value.get("url"), str) or not value["url"].startswith("https://"):
        raise SystemExit(f"invalid {section} URL")
    if not isinstance(value.get("sha256"), str) or not sha256.fullmatch(value["sha256"]):
        raise SystemExit(f"invalid {section} SHA-256")
legal = pins.get("type2_runtime.legal_inputs")
if not isinstance(legal, list) or len(legal) != 11:
    raise SystemExit("invalid Type2 legal inputs")
for item in legal:
    if not isinstance(item, dict) or not re.fullmatch(r"[A-Za-z0-9_.-]+", item.get("name", "")):
        raise SystemExit("invalid Type2 legal input name")
    if not isinstance(item.get("url"), str) or not item["url"].startswith("https://"):
        raise SystemExit("invalid Type2 legal input URL")
    if not isinstance(item.get("sha256"), str) or not sha256.fullmatch(item["sha256"]):
        raise SystemExit("invalid Type2 legal input SHA-256")
    print(f'{item["name"]}\t{item["url"]}\t{item["sha256"]}')
PY
  then
    rm -f "$temporary"
    return 1
  fi
  mv "$temporary" "$target"
}

write_input_manifest() {
  while IFS= read -r -d '' path; do sha256sum "$root/$path"; done < <(git -C "$root" ls-files -co --exclude-standard -z) | sort > "$1"
}

verify_reuse() {
  local expected actual symbol_archive
  test -f "$out/package-inputs.sha256" && test -f "$out/artifacts.sha256" && test -f "$out/inventory.txt" && test -f "$out/symbols/symbol-inventory.json" && test -f "$out/symbols/symbols.sha256" || return 1
  cmp -s "$input_manifest" "$out/package-inputs.sha256" || return 1
  grep -Fqx "source_revision=$revision" "$out/inventory.txt" || return 1
  grep -Fqx "image=$image" "$out/inventory.txt" || return 1
  (cd "$out" && sha256sum -c artifacts.sha256) || return 1
  (cd "$out/symbols" && sha256sum -c symbols.sha256) || return 1
  symbol_archive="FocusForge-$version-x86_64-unknown-linux-gnu-symbols.tar.gz"
  test -f "$out/symbols/$symbol_archive" || return 1
  grep -Fq "  $symbol_archive" "$out/symbols/symbols.sha256" || return 1
  python3 "$root/tools/package-symbols.py" verify "$out/symbols" || return 1
  python3 "$root/tools/package-symbols.py" verify-archive "$out/symbols" "$out/symbols/$symbol_archive" || return 1
  expected=$(printf '%s\n' "FocusForge-$version-x86_64.AppImage" "focusforge_${version}_amd64.deb" | sort)
  actual=$(awk '{print $2}' "$out/artifacts.sha256" | sort) || return 1
  [ "$actual" = "$expected" ] || return 1
  actual=$(find "$out" -maxdepth 1 -type f \( -name '*.AppImage' -o -name '*.deb' \) -printf '%f\n' | sort)
  [ "$actual" = "$expected" ]
}

read_pin_values() {
  python3 - "$1" <<'PY'
import sys
import re
section = None
values = {}
for raw in open(sys.argv[1], encoding="utf-8"):
    line = raw.strip()
    array_header = re.fullmatch(r"\[\[\s*([A-Za-z0-9_.-]+)\s*\]\]\s*(?:#.*)?", line)
    table_header = re.fullmatch(r"\[\s*([A-Za-z0-9_.-]+)\s*\]\s*(?:#.*)?", line)
    if array_header:
        section = None
    elif table_header:
        section = table_header.group(1)
    elif line.startswith("["):
        raise SystemExit("unsupported pin table header")
    elif section and "=" in line:
        key, value = (part.strip() for part in line.split("=", 1))
        values[(section, key)] = value.strip('"')
for item in (("linux", "container"), ("appimagetool", "url"), ("appimagetool", "sha256"), ("type2_runtime", "url"), ("type2_runtime", "sha256")):
    print(values[item])
PY
}

clean_package_output() {
  local path=$1
  if [ -e "$path" ]; then
    docker run --rm -v "$path:/out" "$image" chown -R "$owner_uid:$owner_gid" /out
  fi
  rm -rf "$path"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
if [ "${1:-}" = --write-legal-inputs ]; then
  [ "$#" = 2 ] || [ "$#" = 3 ] || { echo 'usage: package-linux.sh --write-legal-inputs OUTPUT [PINS]' >&2; exit 2; }
  write_legal_inputs "${3:-$pins}" "$2"
  exit $?
fi

mapfile -t pin_values < <(read_pin_values "$pins")
image=${pin_values[0]}
app_url=${pin_values[1]}; app_sha=${pin_values[2]}; runtime_url=${pin_values[3]}; runtime_sha=${pin_values[4]}
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)
revision=$(git -C "$root" rev-parse HEAD)
mkdir -p "$root/.scratch"
input_manifest=$(mktemp "$root/.scratch/package-linux-inputs.XXXXXX")
legal_inputs=$(mktemp "$root/.scratch/package-linux-legal.XXXXXX")
trap 'rm -f "$input_manifest" "$legal_inputs"' EXIT
write_input_manifest "$input_manifest"
write_legal_inputs "$pins" "$legal_inputs"

if [ "${FOCUSFORGE_PACKAGE_REUSE:-}" = 1 ]; then
  verify_reuse || { printf 'refusing package reuse: output is stale, incomplete, or corrupt\n' >&2; exit 1; }
  printf '%s\n' "$out/FocusForge-$version-x86_64.AppImage" "$out/focusforge_${version}_amd64.deb"
  exit 0
fi

next="$out.next"
owner_uid=$(id -u)
owner_gid=$(id -g)
clean_package_output "$next"
mkdir -p "$next"
set +e
docker run --rm -i -v "$root:/src:ro" -v "$next:/out" -v "$input_manifest:/inputs/package-inputs.sha256:ro" -v "$legal_inputs:/inputs/type2-legal-inputs.tsv:ro" "$image" bash -s -- "$version" "$app_url" "$app_sha" "$runtime_url" "$runtime_sha" "$revision" "$image" "$owner_uid" "$owner_gid" <<'CONTAINER'
set -euo pipefail
version=$1; app_url=$2; app_sha=$3; runtime_url=$4; runtime_sha=$5; revision=$6; image=$7; owner_uid=${8}; owner_gid=${9}
deb_version=${version/-/\~}
trap 'chown -R "$owner_uid:$owner_gid" /out' EXIT
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential ca-certificates curl xz-utils file binutils dpkg-dev squashfs-tools pkg-config libfontconfig1-dev python3 > /out/apt-install.log
dpkg_version=$(dpkg-deb --version | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
test "$dpkg_version" = 1.21.1
fetch() { curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 -o "$2" "$1"; }
fetch "$app_url" /out/appimagetool.AppImage
printf '%s  %s\n' "$app_sha" /out/appimagetool.AppImage | sha256sum -c -
fetch "$runtime_url" /out/runtime-x86_64
printf '%s  %s\n' "$runtime_sha" /out/runtime-x86_64 | sha256sum -c -
mkdir -p /out/type2-legal-inputs
cp /inputs/type2-legal-inputs.tsv /out/type2-legal-inputs.tsv
while IFS=$'\t' read -r name url sha; do
  fetch "$url" "/out/type2-legal-inputs/$name"
  printf '%s  %s\n' "$sha" "/out/type2-legal-inputs/$name" | sha256sum -c -
done < /out/type2-legal-inputs.tsv
chmod +x /out/appimagetool.AppImage
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /out/rustup-init.sh
sh /out/rustup-init.sh -y --default-toolchain 1.95.0 --profile minimal
. "$HOME/.cargo/env"
mkdir /work
(cd /src && tar --exclude='./target' --exclude='./.scratch' --exclude='./.logs' --exclude='./.locks' -cf - .) | tar -C /work -xf -
cd /work
# One read-and-execute segment instead of a read-only one followed by an
# executable one: with separate segments the loader can leave a page-sized
# hole between them, which the crash writer then reports as a second module
# whose base is wrong, so no frame of a crash in the binary resolves.
RUSTFLAGS='-C link-arg=-Wl,--no-rosegment' CARGO_TARGET_DIR=/out/target cargo build --release --locked -p focusforge-cli -p focusforge-gui |& tee /out/cargo-build.log
for bin in focusforge-cli focusforge-gui; do test -x "/out/target/release/$bin"; done
dump_syms=$(python3 tools/package-symbols.py install dump_syms x86_64-unknown-linux-gnu /out/symbol-tools)
install -d /out/original /out/native-debug
for bin in focusforge-cli focusforge-gui; do
  cp "/out/target/release/$bin" "/out/original/$bin"
  objcopy --only-keep-debug "/out/original/$bin" "/out/native-debug/$bin.debug"
  objcopy --strip-debug --add-gnu-debuglink="/out/native-debug/$bin.debug" "/out/target/release/$bin"
done
python3 tools/package-symbols.py build --platform x86_64-unknown-linux-gnu --version "$version" --dump-syms "$dump_syms" --output /out/symbols --output-root /out \
  --native focusforge-cli=/out/native-debug/focusforge-cli.debug --native focusforge-gui=/out/native-debug/focusforge-gui.debug \
  --binary focusforge-cli=/out/original/focusforge-cli --binary focusforge-gui=/out/original/focusforge-gui
python3 tools/package-symbols.py verify-linux --root /out/symbols \
  --identity focusforge-cli=/out/original/focusforge-cli,/out/target/release/focusforge-cli,/out/native-debug/focusforge-cli.debug \
  --identity focusforge-gui=/out/original/focusforge-gui,/out/target/release/focusforge-gui,/out/native-debug/focusforge-gui.debug
install -d /out/AppDir/usr/bin /out/AppDir/usr/share/applications /out/AppDir/usr/share/icons/hicolor /out/AppDir/usr/share/doc/focusforge
install -m 0755 /out/target/release/focusforge-{cli,gui} /out/AppDir/usr/bin/
install -m 0644 LICENSE NOTICE /out/AppDir/usr/share/doc/focusforge/
install -m 0644 assets/focusforge.desktop /out/AppDir/usr/share/applications/focusforge.desktop
cp -a assets/build/hicolor/. /out/AppDir/usr/share/icons/hicolor/
python3 tools/package-cli-assets.py stage /work /out/AppDir/usr
install -d /out/AppDir/usr/lib
runtime_libs=/out/runtime-libs.txt
: > "$runtime_libs"
for binary in /out/AppDir/usr/bin/focusforge-cli /out/AppDir/usr/bin/focusforge-gui; do
  ldd "$binary" | awk '/=> \/[^ ]+/ { print $3 } /^\// { print $1 }' >> "$runtime_libs"
done
sort -u "$runtime_libs" -o "$runtime_libs"
: > /out/AppDir/usr/share/doc/focusforge/native-notices.txt
printf '%s\n' 'Bundled native runtime payloads (Debian package provenance):' >> /out/AppDir/usr/share/doc/focusforge/native-notices.txt
install -d /out/AppDir/usr/share/doc/focusforge/native-licenses
while IFS= read -r library; do
  case "$(basename "$library")" in
    ld-linux-*|libc.so.*|libdl.so.*|libm.so.*|libpthread.so.*|librt.so.*|libresolv.so.*|libutil.so.*) continue ;;
  esac
  install -m 0644 "$library" "/out/AppDir/usr/lib/$(basename "$library")"
  owner=$({ dpkg-query -S "$(readlink -f "$library")" 2>/dev/null || dpkg-query -S "$library"; } | head -1 | sed 's/: .*//')
  dpkg-query -W -f '${Package}\t${Version}\t${Homepage}\n' "$owner" >> /out/AppDir/usr/share/doc/focusforge/native-notices.txt
  copyright="/usr/share/doc/${owner%%:*}/copyright"
  test -f "$copyright"
  cp "$copyright" "/out/AppDir/usr/share/doc/focusforge/native-licenses/${owner%%:*}.copyright"
  while IFS= read -r common; do
    test -f "$common"
    cp "$common" "/out/AppDir/usr/share/doc/focusforge/native-licenses/$(basename "$common")"
  done < <(grep -Eo '/usr/share/common-licenses/[A-Za-z0-9.+-]+' "$copyright" | sed 's/\.$//' | sort -u)
done < "$runtime_libs"
type2_docs=/out/AppDir/usr/share/doc/focusforge/type2-runtime
install -d "$type2_docs/notices" "$type2_docs/sources"
printf '%s\n' 'Type2 runtime 20251108 (commit dd6cebedcbddde9c82f89b011e8e1d40b6e43868)' 'This AppImage embeds the supplied Type2 runtime and carries its complete versioned notice inputs below.' > "$type2_docs/THIRD-PARTY-NOTICES.txt"
while IFS=$'\t' read -r name url sha; do
  printf '\n%s\nsource: %s\nsha256: %s\n\n' "$name" "$url" "$sha" >> "$type2_docs/THIRD-PARTY-NOTICES.txt"
  case "$name" in
    *.tar.gz) cp "/out/type2-legal-inputs/$name" "$type2_docs/sources/$name" ;;
    *) cp "/out/type2-legal-inputs/$name" "$type2_docs/notices/$name"; cat "/out/type2-legal-inputs/$name" >> "$type2_docs/THIRD-PARTY-NOTICES.txt" ;;
  esac
done < /out/type2-legal-inputs.tsv
cat > "$type2_docs/RELINKING.md" <<'RELINK'
The embedded runtime is `runtime-x86_64` from Type2 runtime commit
`dd6cebedcbddde9c82f89b011e8e1d40b6e43868`.  Its complete source archive and
the exact libfuse 3.15.0 source archive are in `sources/`.

To build a modified x86_64 runtime, unpack the Type2 and libfuse archives.
Place the libfuse tree at `vendor/libfuse`, apply
`patch -p1 < patches/libfuse/mount.c.diff`, and add
`COPY vendor/libfuse/ /tmp/vendor/libfuse/` to the Dockerfile. In
`scripts/docker/install-dependencies.sh`, replace only the libfuse
download/checksum/extract/upstream-patch block with a copy from
`/tmp/vendor/libfuse` to its temporary build directory and `cd` there; retain
the Meson static build and Ninja install. From the runtime root, run
`ARCH=x86_64 scripts/docker/build-with-docker.sh`. Replace the resulting
`runtime-x86_64` by running this package recipe with an updated,
SHA-256-verified runtime pin.
RELINK
printf '%s\n' 'AppImage Type2 runtime notices, source archives and relinking instructions: usr/share/doc/focusforge/type2-runtime/' >> /out/AppDir/usr/share/doc/focusforge/native-notices.txt
ln -s usr/share/applications/focusforge.desktop /out/AppDir/focusforge.desktop
ln -s usr/share/icons/hicolor/256x256/apps/focusforge.png /out/AppDir/focusforge.png
printf '%s\n' '#!/bin/sh' 'here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)' 'export LD_LIBRARY_PATH="$here/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"' 'exec "$here/usr/bin/focusforge-gui" "$@"' > /out/AppDir/AppRun
chmod 0755 /out/AppDir/AppRun
install -d /out/deb/usr/bin /out/deb/usr/share/applications /out/deb/usr/share/icons/hicolor /out/deb/usr/share/doc/focusforge /out/deb/DEBIAN
install -m 0755 /out/target/release/focusforge-{cli,gui} /out/deb/usr/bin/
install -m 0644 LICENSE NOTICE /out/deb/usr/share/doc/focusforge/
install -m 0644 assets/focusforge.desktop /out/deb/usr/share/applications/focusforge.desktop
cp -a assets/build/hicolor/. /out/deb/usr/share/icons/hicolor/
python3 tools/package-cli-assets.py stage /work /out/deb/usr
mkdir -p /out/deb/DEBIAN /tmp/debian
printf '%s\n' 'Source: focusforge' 'Section: graphics' 'Priority: optional' 'Maintainer: FocusForge <release@focusforge.invalid>' 'Standards-Version: 4.6.0' '' 'Package: focusforge' 'Architecture: any' 'Description: Focus stacking desktop and command-line tools' > /tmp/debian/control
depends=$(cd /tmp && dpkg-shlibdeps -O -e/out/deb/usr/bin/focusforge-cli -e/out/deb/usr/bin/focusforge-gui | sed -n 's/^shlibs:Depends=//p')
test -n "$depends"
printf '%s\n' "Package: focusforge" "Version: $deb_version" 'Architecture: amd64' 'Maintainer: FocusForge' 'License: GPL-3.0-only' "Depends: $depends" 'Description: Focus stacking desktop and command-line tools' > /out/deb/DEBIAN/control
printf '%s\n' 'Native libraries resolved at package build by dpkg-shlibdeps:' "$depends" > /out/deb/usr/share/doc/focusforge/native-notices.txt
dpkg-deb --build --root-owner-group /out/deb "/out/focusforge_${version}_amd64.deb" |& tee /out/dpkg-build.log
ARCH=x86_64 VERSION="$version" APPIMAGE_EXTRACT_AND_RUN=1 /out/appimagetool.AppImage --runtime-file /out/runtime-x86_64 /out/AppDir "/out/FocusForge-${version}-x86_64.AppImage" |& tee /out/appimage-build.log
(cd /out && sha256sum "FocusForge-${version}-x86_64.AppImage" "focusforge_${version}_amd64.deb") | tee /out/artifacts.sha256
cp /inputs/package-inputs.sha256 /out/package-inputs.sha256
printf 'source_revision=%s\nimage=%s\ndpkg_deb=%s\nappimagetool=1.9.1 sha256=%s\ntype2_runtime=20251108 sha256=%s\n' "$revision" "$image" "$dpkg_version" "$app_sha" "$runtime_sha" > /out/inventory.txt
CONTAINER
status=$?
set -e
[ "$status" = 0 ] || exit "$status"
clean_package_output "$out"
mv "$next" "$out"
printf '%s\n' "$out/FocusForge-$version-x86_64.AppImage" "$out/focusforge_${version}_amd64.deb"
fi
