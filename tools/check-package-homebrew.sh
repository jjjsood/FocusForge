#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Usage: tools/check-package-homebrew.sh CASK DMG VERSION
#
# On macOS with Homebrew: puts the cask that tools/package-managers.py
# rendered into a throwaway local tap, runs `brew style` on it, then installs
# it from DMG -- the cask with only its url pointed at the local file, so its
# sha256 is checked against the disk image a pull request built -- and checks
# the app, the linked CLI and its manual pages and completions before
# uninstalling with --zap. Nothing is downloaded and nothing is pushed.
set -euo pipefail

[ "$#" -eq 3 ] || { echo "usage: $0 CASK DMG VERSION" >&2; exit 2; }
cask=$1 dmg=$2 version=$3
[ -f "$cask" ] && [ -f "$dmg" ] || { echo "missing $cask or $dmg" >&2; exit 2; }
command -v brew >/dev/null || { echo "Homebrew is required" >&2; exit 2; }
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_INSTALL_CLEANUP=1

tap=focusforge-check/local
name=$(basename "$cask" .rb)
brew tap-new --no-git "$tap" >/dev/null
trap 'brew untap --force "$tap" >/dev/null 2>&1 || :' EXIT
casks="$(brew --repository "$tap")/Casks"
mkdir -p "$casks"
cp "$cask" "$casks/$name.rb"
brew style --cask "$tap/$name"

dmg_url="file://$(cd "$(dirname "$dmg")" && pwd)/$(basename "$dmg")"
python3 - "$casks/$name.rb" "$dmg_url" <<'PY'
import re
import sys
from pathlib import Path

path, url = Path(sys.argv[1]), sys.argv[2]
text, count = re.subn(r'^  url "[^"]*"$', f'  url "{url}"', path.read_text(), flags=re.M)
if count != 1:
    raise SystemExit("the cask must have exactly one url line")
path.write_text(text)
PY
brew install --cask "$tap/$name"

app=/Applications/FocusForge.app
[ -d "$app" ] || { echo "the cask did not install $app" >&2; exit 1; }
# What the caveats tell a user to do for a build that is not notarized.
xattr -dr com.apple.quarantine "$app"
prefix=$(brew --prefix)
reported=$("$prefix/bin/focusforge-cli" --version)
[ "$reported" = "focusforge-cli $version" ] || { echo "focusforge-cli reports '$reported', expected $version" >&2; exit 1; }
for path in "$prefix/share/man/man1/focusforge-cli.1.gz" "$prefix/share/man/man1/focusforge-cli-stack.1.gz" \
            "$prefix/etc/bash_completion.d/focusforge-cli" "$prefix/share/zsh/site-functions/_focusforge-cli" \
            "$prefix/share/fish/vendor_completions.d/focusforge-cli.fish"; do
  [ -e "$path" ] || { echo "the cask does not link $path" >&2; exit 1; }
done

brew uninstall --cask --zap "$tap/$name"
[ ! -e "$app" ] && [ ! -e "$prefix/bin/focusforge-cli" ] || { echo "uninstall left the app or the CLI link behind" >&2; exit 1; }
echo "OK    cask $name $version: style, install, CLI, manuals, completions and zap"
