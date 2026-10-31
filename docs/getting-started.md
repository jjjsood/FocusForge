# Getting started

FocusForge is always available from source. Release artifacts are optional:
use one only when its matching release provides it, otherwise follow the
[source build](../README.md#build).

## Linux

A Linux release may provide `FocusForge-<version>-x86_64.AppImage` and
`focusforge_<version>_amd64.deb`. Replace `<version>` with the release version
before pasting a command.

```bash
sudo apt install "./focusforge_<version>_amd64.deb"
focusforge-cli stack path/to/frames/ --algorithm laplacian
```

The Debian package installs `focusforge-cli` at `/usr/bin/focusforge-cli` and
its shared resources under `/usr/share`. To use the AppImage, make it
executable and run it. It contains the CLI internally at
`usr/bin/focusforge-cli` after extraction; it does not install that path on
the host.

```bash
chmod +x "FocusForge-<version>-x86_64.AppImage"
"./FocusForge-<version>-x86_64.AppImage"
```

## macOS and Windows

A macOS release may provide `FocusForge-<version>-aarch64.dmg`. Open the
disk image and drag `FocusForge.app` to `Applications`. Its CLI is
`/Applications/FocusForge.app/Contents/MacOS/focusforge-cli`, and its shared
resources are below `/Applications/FocusForge.app/Contents/Resources/share`.

macOS releases are signed ad hoc and are not notarized, so macOS checks a
downloaded copy with Gatekeeper the first time it opens. If macOS reports that
“FocusForge” is damaged and can’t be opened, or offers no way to open it, copy
the app to `Applications`, run this in Terminal and open it normally:

```sh
xattr -cr /Applications/FocusForge.app
```

This removes the quarantine flag the browser set on the download and works on
every macOS version. If macOS instead says it cannot verify the developer, open
System Settings → Privacy & Security and choose Open Anyway for FocusForge.

A Windows release may provide `FocusForge-<version>-x86_64.msi`. Run the
installer and follow its prompts. Its CLI is
`$env:ProgramFiles\FocusForge\focusforge-cli.exe`; the installer does not add
it to `PATH`. Its shared resources are below
`$env:ProgramFiles\FocusForge\share`.

## Manuals and completions

Every package contains 15 manual pages, including `focusforge-cli-stack(1)`,
and completion scripts for Bash, Zsh, Fish and PowerShell. On Linux their
paths are:

```text
/usr/share/man/man1/focusforge-cli*.1.gz
/usr/share/bash-completion/completions/focusforge-cli
/usr/share/zsh/vendor-completions/_focusforge-cli
/usr/share/fish/vendor_completions.d/focusforge-cli.fish
/usr/share/powershell/completions/_focusforge-cli.ps1
```

For an extracted AppImage, replace `/usr/share` with `usr/share`; for macOS,
replace it with `/Applications/FocusForge.app/Contents/Resources/share`; for
Windows, replace it with `$env:ProgramFiles\FocusForge\share`.

Use the manual page without changing the system man database:

```bash
man -M /usr/share/man focusforge-cli-stack
```

Load a completion script for the current shell session:

```bash
# Bash
source /usr/share/bash-completion/completions/focusforge-cli
```

```zsh
# Zsh
fpath=(/usr/share/zsh/vendor-completions $fpath); autoload -Uz compinit; compinit
```

```fish
# Fish
source /usr/share/fish/vendor_completions.d/focusforge-cli.fish
```

```powershell
# PowerShell, in an existing pwsh session
. /usr/share/powershell/completions/_focusforge-cli.ps1
# Windows package:
. "$env:ProgramFiles\FocusForge\share\powershell\completions\_focusforge-cli.ps1"
```

Copy or source only the script for the shell you use; do not modify a profile
until you have checked the package path on that operating system. Continue
with the [stacking guide](guide/stacking.md) for the command workflow.
