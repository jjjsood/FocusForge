# Installing FocusForge

FocusForge comes as one package per operating system. Each package holds the
desktop application, which this manual describes, and the `focusforge-cli`
command-line program, which does the same work from a terminal.

Release packages exist for the systems listed under
[What to download](#what-to-download). For any other system, or to build
FocusForge yourself, see
[`CONTRIBUTING.md`](https://github.com/jjjsood/FocusForge/blob/main/CONTRIBUTING.md)
in the repository.

## What your computer needs

- **Free RAM.** What counts is the memory that is free when you fuse, not the
  memory installed. Plan on about 4 GB free for a stack of TIFF files, 5 GB
  for JPEG, 8 GB for camera RAW from a 24 to 26 megapixel camera and 15 GB
  for camera RAW from a 45 to 61 megapixel one. The number of frames hardly
  matters.
- **Graphics card.** Optional. FocusForge uses one when it finds one and gives
  the same result without, only more slowly (see
  [Machines without a GPU](#machines-without-a-gpu)).
- **Disk space.** Room for your project files and exported images. A camera
  RAW stack that does not fit in memory also uses temporary files, 160 to
  370 MB per frame. FocusForge checks the free space before a run and tells
  you if it is too little ([Not enough free
  space](troubleshooting.md#not-enough-free-space)).
- **Files.** FocusForge opens TIFF, JPEG and camera RAW frames and never
  changes them. It writes project files (`.ffp`), the images you export
  (TIFF, PNG, JPEG, and DNG for a RAW project) and, in its own folders, its
  settings, logs and temporary files (see
  [Storage](guide/settings-window.md#storage)). The full list of readable
  files is in [Which files can FocusForge
  read?](troubleshooting.md#which-files-can-focusforge-read).

## What to download

Open the [Releases page](https://github.com/jjjsood/FocusForge/releases),
choose a release and download the file for your computer. `<version>` stands
for the release number, for example `0.1.0`.

| Your computer | File | What it is |
|---|---|---|
| Linux (Debian, Ubuntu and relatives), 64-bit Intel/AMD | `focusforge_<version>_amd64.deb` | A package your system installs, with a menu entry |
| Any other 64-bit Intel/AMD Linux | `FocusForge-<version>-x86_64.AppImage` | One file that runs without installing |
| Mac with Apple silicon (M1 and later) | `FocusForge-<version>-aarch64.dmg` | A disk image holding `FocusForge.app` |
| Windows, 64-bit | `FocusForge-<version>-x86_64.msi` | An installer |

> **Note:** A release lists only the files it was built with. Nothing in this
> manual depends on how you installed.

## Linux

### The `.deb` package

Install it with your package manager, from the folder you downloaded it to.
Replace `<version>` with the release number before pasting the command:

```bash
sudo apt install "./focusforge_<version>_amd64.deb"
```

**FocusForge** then appears in your desktop's application menu, under
Graphics or Photography. The package installs the window as
`/usr/bin/focusforge-gui`, the command-line program as
`/usr/bin/focusforge-cli`, and its shared resources (menu entry, icons,
manual pages, completions) under `/usr/share`.

### The AppImage

An AppImage needs no installation. Make it executable once, then run it:

```bash
chmod +x "FocusForge-<version>-x86_64.AppImage"
"./FocusForge-<version>-x86_64.AppImage"
```

Running the AppImage opens the window. It contains the command-line program
internally at `usr/bin/focusforge-cli` (visible after extracting the
AppImage); it does not install that path on your system.

## macOS

1. Open `FocusForge-<version>-aarch64.dmg`.
2. Drag `FocusForge.app` to `Applications`.
3. Start **FocusForge** from Applications or Launchpad.

The command-line program sits inside the app at
`/Applications/FocusForge.app/Contents/MacOS/focusforge-cli`, and the shared
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

## Windows

Run `FocusForge-<version>-x86_64.msi` and follow its prompts; the programs
carry their C runtime, so no Visual C++ redistributable is needed. Afterwards
**FocusForge** is in the Start menu.

The installer puts everything in `$env:ProgramFiles\FocusForge`: the window
as `focusforge-gui.exe`, the command-line program as `focusforge-cli.exe`
and the shared resources below `$env:ProgramFiles\FocusForge\share`. It does
not add the folder to `PATH`, so call the command-line program by its full
path.

> **Note:** If Windows SmartScreen stops the installer, choose
> **More info**, then **Run anyway**.

## Machines without a GPU

`focusforge-gui` tries GPU-backed graphics adapters first and falls back to a
software adapter (for example in a virtual machine or a Remote Desktop
session), logging a warning that names it. It runs there at software speed.
`focusforge-cli stack` and the stacking in the window never compute on a software
adapter: fusion and alignment run on the CPU, and the log names the adapter.
When no adapter works at all it exits with an error; on Windows, started from
the Start menu, it shows that error in a message box.

`focusforge-gui --version` prints the program name and its version, for
example `focusforge-gui 0.1.0`, and exits without opening a window.

On Windows, `focusforge-gui --help`, `--version` and argument errors print to
the terminal that started the program, including when its output is
redirected. PowerShell does not wait for a windowed program, so there pipe
the output instead: `focusforge-gui --help | Out-File help.txt`, or
`focusforge-gui --no-such-flag 2>&1 | Out-String` to get `$LASTEXITCODE`.

## First launch

Start FocusForge from your application menu, Launchpad or Start menu (or
run `focusforge-gui` in a terminal). The window opens with no project:

![The FocusForge window on its first start: the FocusForge wordmark above a card reading "No project open" with an Open… button and the caption "Drop a folder or stack"](assets/manual/first-stack/empty-window.png)

*The window on its first start, ready for a stack.*

From here, [Your first stack](guide/first-stack.md) walks you through opening
your frames, fusing them and keeping the result.

Each start, FocusForge asks the release server once, in the background,
whether a newer release exists, and tells you with a notification if there
is one. Nothing is downloaded or installed. You can switch the check off, or
run it yourself, in [Updates](guide/settings-window.md#updates); the command
line has `focusforge-cli check-update`.

> **Tip:** On Linux, when you start FocusForge from a launcher that shows a
> busy cursor, the window tells the desktop as soon as it has appeared, so the
> busy cursor ends right away. (It uses the startup token the launcher passes
> in `DESKTOP_STARTUP_ID` or `XDG_ACTIVATION_TOKEN`.)

## For the command line

Everything the window does with a stack can also be scripted with
`focusforge-cli`. For example, after installing the `.deb`:

```bash
focusforge-cli stack path/to/frames/ --algorithm laplacian
```

Every package contains 18 manual pages, including `focusforge-cli-stack(1)`,
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

Read a manual page without changing the system man database:

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

Copy or source only the script for the shell you use, and check the package
path on your operating system before you add it to a shell profile. Every
command and option is described in the
[command-line reference](reference/cli.md).
