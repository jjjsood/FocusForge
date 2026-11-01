# Diagnosing a failed or crashed run

Every run of `focusforge-cli` or the GUI keeps a record of what it did, and
if it crashes, keeps evidence of that too — all of it local to your own
machine. This page covers reading that record and handing it to someone
else without also handing over your home directory or your images.

## The log

Alongside whatever `-v`/`-q` print on stderr, every run appends to a JSON
Lines log file in the platform's per-user data directory, rotating once it
reaches 8 MiB and keeping up to five rotated files. See
[`reference/cli.md`](../reference/cli.md#global-options) for what each entry
carries and how `-v`/`-q` change stderr's own detail.

## When something crashes

A panic — the process catching its own failure and unwinding — writes a
crash report next to the log: the panic message, a backtrace, and a short
recording of the most recent log activity leading up to it, so a crash a
user can't reproduce still comes with context.

A crash the process cannot catch by unwinding — a segfault, a stack
overflow, an abort — is captured a different way: the process restarts
itself as a small watcher before doing any work, and if the crash happens,
that watcher writes a minidump plus a JSON file naming the binary, its
version, the process id and the signal.

Either way, nothing is sent anywhere. The evidence sits in the data
directory until you package it yourself.

## The crash notice

The next time you start `focusforge-cli` after a crash, it prints a one-line
hint before doing anything else. The hint clears itself once you run
`focusforge-cli diagnostics` — see
below — which marks every crash it packages as reported.

The GUI records the same thing as a notification the next time you start it,
naming how many crashes it found, and shows it as a toast and in the
notification center (see [Notifications](gui.md#notifications)). Reading the
notice, letting its toast expire or closing the center does not mark the
evidence reported, and it stays in the center for the session. Two explicit
actions on the selected crash notice do: **Acknowledge** marks every listed
crash reported, and **Diagnostics** acknowledges them and opens
Settings → Diagnostics. Creating a bundle there also marks that evidence
reported, so it does not come back at the next start. If the mark cannot be
written, the notice says so and the evidence stays unreported; if the crash
folder itself cannot be read, the notice reads "Crash reports unavailable"
and Diagnostics is still reachable. While the window is in the background the
notice also arrives as an operating-system banner with the title and a short
context only.

## Packaging a diagnostics bundle

```
focusforge-cli diagnostics [-o out.zip] [--include-minidumps] [--force]
```

Collects the log files, any crash reports, a system summary, the graphics
adapter record, the configuration a new run would resolve right now (with
where each value came from), and the version and commit into one ZIP. It
prints its planned contents before writing it.

Every text entry has your home directory path and user name stripped out;
no image data is ever included. Minidumps are left out by default — pass
`--include-minidumps` to add them, and each one is marked unredacted in the
listing, since a minidump holds module paths and raw stack memory from the
crashed process. `-o` names the output path (default: a dated ZIP in the
current directory), and `--force` overwrites a path that already exists.

### From the window

Settings → Diagnostics shows the same file list `focusforge-cli diagnostics`
prints for your data directory, each row's size in kilobytes below a
megabyte and one decimal above. The list gathers that in the background, so
it opens showing "Collecting…" rather than blocking the window, and the
export button stays unavailable until it finishes; the minidump toggle and
destination field stay editable throughout. "Include minidumps" is off by
default; switch it on to add them to the bundle -- doing so re-gathers the
list, since which files it would collect just changed -- and every minidump
row is marked not redacted in the list, with a warning line explaining why.

The destination field defaults to a dated ZIP beside your logs, and is
editable. Creating the bundle checks the path first: a destination that
already exists asks you to overwrite it before writing anything, and a
destination that cannot be created (a bad path, a full disk) reports the
failure with nothing left behind. The write runs in the background, so the
window stays responsive while it does. A finished bundle can be exported
again immediately — a fresh destination is filled in automatically, so
there is no extra step between one export and the next.

## Profiling the window

The GUI can write a Chrome-trace file of its own activity: input, redraws,
canvas publication, saves and stroke renders. Build it with the `trace`
feature and name the output file:

```
cargo build --release -p focusforge-gui --features trace
FOCUSFORGE_TRACE_FILE=gui-trace.json target/release/focusforge-gui
```

Open the file in `ui.perfetto.dev` or `chrome://tracing`. Window spans draw
on their own admission budget, so a long session cannot starve the
processing spans of a fuse running in the same process. When the run ends,
a `timing_summary` span records each budget, how many spans it admitted and
how many it refused.

To see Slint's own frame timing as well, set
`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` in the same environment. Slint
collects and prints refresh statistics when a frame is rendered; `refresh_lazy`
does not force frames while the window is idle.

`focusforge-cli` takes the same feature and variable; see the repository's
`CONTRIBUTING.md` for the command.
