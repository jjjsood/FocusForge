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
hint before doing anything else:

```
$ focusforge-cli info some/folder
2026-09-18T17:07:09Z ERROR focusforge_cli: 1 earlier run crashed -- run focusforge-cli diagnostics to package the evidence
```

The hint clears itself once you run `focusforge-cli diagnostics` — see
below — which marks every crash it packages as reported.

## Packaging a diagnostics bundle

```
focusforge-cli diagnostics [-o out.zip] [--include-minidumps] [--force]
```

Collects the log files, any crash reports, a system summary, the graphics
adapter record, the configuration a new run would resolve right now (with
where each value came from), and the version and commit into one ZIP. It
prints what it's about to write before writing it:

```
$ focusforge-cli diagnostics
diagnostics bundle contents:
  adapter.json (generated, redacted)
  configuration.json (generated, redacted)
  system.json (generated, redacted)
  version.json (generated, redacted)
  logs/focusforge.jsonl (log, redacted)
no crash reports were found
wrote focusforge-diagnostics-20260918.zip
```

Every text entry has your home directory path and user name stripped out;
no image data is ever included. Minidumps are left out by default — pass
`--include-minidumps` to add them, and each one is marked unredacted in the
listing, since a minidump holds module paths and raw stack memory from the
crashed process. `-o` names the output path (default: a dated ZIP in the
current directory), and `--force` overwrites a path that already exists.

### From the window

Settings → Diagnostics shows the same file list `focusforge-cli diagnostics`
prints for your data directory, each row's size in kilobytes below a
megabyte and one decimal above. "Include minidumps" is off by default; switch
it on to add them to the bundle, and every minidump row is marked not
redacted in the list, with a warning line explaining why.

The destination field defaults to a dated ZIP beside your logs, and is
editable. Creating the bundle checks the path first: a destination that
already exists asks you to overwrite it before writing anything, and a
destination that cannot be created (a bad path, a full disk) reports the
failure with nothing left behind. The write runs in the background, so the
window stays responsive while it does. A finished bundle can be exported
again immediately — a fresh destination is filled in automatically, so
there is no extra step between one export and the next.
