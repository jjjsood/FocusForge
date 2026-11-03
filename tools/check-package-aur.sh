#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/check-package-aur.sh PKGBUILD_DIR DEB VERSION OUT_DIR
#
# Builds, installs and removes the AUR package that tools/package-managers.py
# rendered, on Arch Linux, as root (the release workflow runs it in an
# archlinux:base-devel container). DEB is the release .deb: makepkg finds it
# under the PKGBUILD's source name and checks it against the PKGBUILD's
# sha256sums, so no download happens and a pull request without a published
# release checks the same recipe a tag pushes. On success OUT_DIR holds the
# PKGBUILD and the .SRCINFO that makepkg derived from it -- the two files the
# AUR repository receives.
#
# Fails when the checksum differs, a dependency name is not an Arch package,
# a library the binaries link or load is not on the installed system, the CLI
# reports another version, or the package leaves files behind on removal.
set -euo pipefail

[ "$#" -eq 4 ] || { echo "usage: $0 PKGBUILD_DIR DEB VERSION OUT_DIR" >&2; exit 2; }
recipe=$1 deb=$2 version=$3 out=$4
[ "$(id -u)" = 0 ] || { echo "run as root on Arch Linux" >&2; exit 2; }
[ -f "$recipe/PKGBUILD" ] || { echo "missing $recipe/PKGBUILD" >&2; exit 2; }
[ -f "$deb" ] || { echo "missing $deb" >&2; exit 2; }

# A partial upgrade is unsupported on Arch: refresh the keyring and the system before installing anything.
pacman -Sy --noconfirm --needed archlinux-keyring
pacman -Su --noconfirm
pacman -S --noconfirm --needed namcap

id builder >/dev/null 2>&1 || useradd --create-home builder
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$recipe/PKGBUILD" "$work/PKGBUILD"
source_name=$(cd "$work" && bash -c 'source ./PKGBUILD && [ "${#source[@]}" -eq 1 ] && printf %s "${source[0]%%::*}"')
pkgname=$(cd "$work" && bash -c 'source ./PKGBUILD && printf %s "$pkgname"')
[ -n "$source_name" ] && [ -n "$pkgname" ] || { echo "PKGBUILD must name one source and one package" >&2; exit 1; }
cp "$deb" "$work/$source_name"
chown -R builder: "$work"

# --nodeps: the package is built from a .deb and needs nothing to build; its run-time
# dependencies are resolved by pacman below, which fails on a name Arch does not have.
su builder -c "cd '$work' && makepkg --printsrcinfo > .SRCINFO && makepkg --nodeps --noconfirm"
package=$(find "$work" -maxdepth 1 -name "$pkgname-*.pkg.tar.zst" -print)
[ "$(printf '%s\n' "$package" | grep -c .)" = 1 ] || { echo "expected one built package, found: $package" >&2; exit 1; }

echo "namcap (advisory):"
namcap "$work/PKGBUILD" "$package" 2>&1 | sed 's/^/  /' || echo "  namcap exited non-zero; its findings are advisory"

pacman -U --noconfirm "$package"

reported=$(focusforge-cli --version)
[ "$reported" = "focusforge-cli $version" ] || { echo "focusforge-cli reports '$reported', expected $version" >&2; exit 1; }
missing=$(ldd /usr/bin/focusforge-cli /usr/bin/focusforge-gui | grep 'not found' || :)
[ -z "$missing" ] || { echo "unresolved linked libraries:"; echo "$missing"; exit 1; } >&2
# winit, wgpu and the notification path open these with dlopen, which ldd does not see.
loaded=$(grep -ahoE 'lib[A-Za-z0-9_+-]{1,40}\.so\.[0-9]+' /usr/bin/focusforge-cli /usr/bin/focusforge-gui | sort -u)
cache=$(ldconfig -p)
absent=()
for soname in $loaded; do
  grep -qE "^[[:space:]]+${soname//./\\.} " <<< "$cache" || absent+=("$soname")
done
[ "${#absent[@]}" = 0 ] || { echo "no installed package provides: ${absent[*]} (add it to AUR_DEPENDS in tools/package-managers.py)" >&2; exit 1; }
for path in /usr/bin/focusforge-gui /usr/share/applications/focusforge.desktop /usr/share/man/man1/focusforge-cli.1.gz \
            /usr/share/bash-completion/completions/focusforge-cli /usr/share/zsh/site-functions/_focusforge-cli \
            /usr/share/fish/vendor_completions.d/focusforge-cli.fish; do
  [ -f "$path" ] || { echo "package does not install $path" >&2; exit 1; }
done

owned=$(pacman -Qlq "$pkgname" | grep -v '/$')
pacman -R --noconfirm "$pkgname"
while IFS= read -r path; do
  [ ! -e "$path" ] || { echo "removal left $path behind" >&2; exit 1; }
done <<< "$owned"

mkdir -p "$out"
cp "$work/PKGBUILD" "$work/.SRCINFO" "$out/"
echo "OK    $pkgname $version: built, installed, run and removed; $(printf '%s\n' "$loaded" | wc -l) sonames resolved"
