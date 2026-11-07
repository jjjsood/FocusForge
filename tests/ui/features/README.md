# Features

Each feature file says how a user reaches the feature, which controls matter, and which journeys cover it (one to three per feature). Every fixture fuse ends in the keep question ("Keep this fused result?"), because the fixture frames never align fully (except `fixtures/open-rejected`, whose fuse fails); journeys answer **Keep** as their own step. A kept fuse lands in the editor.

| Feature | Journeys | Covers |
|---|---|---|
| [fuse-stack](fuse-stack.md) | 01, 02, 16 | fuse, keep or discard, not-fused state, pause, resume, cancel, queued tab, cancelled-run record, failed run and Retry |
| [fuse-stage-view](fuse-stage-view.md) | 20 | the Composite pane during a run: "Not fused yet" card, "Fusing" stage card kept through pause and resume, the other tab, the pane in its own window, cancel, discard, keep, fuse over a kept result |
| [retouch](retouch.md) | 03, 04 | editor mode, brush strokes, size and softness, undo and redo, source frame, save checkpoint, fuse-again and restore questions, select and delete checkpoints |
| [checkpoints-export](checkpoints-export.md) | 04, 05 | CHECKPOINTS list, the export dialog and its states |
| [close-and-quit](close-and-quit.md) | 06 | close-tab and quit questions, Quit without saving |
| [navigation-and-settings](navigation-and-settings.md) | 07 | dock tabs, zoom, two tabs, Z-Map overlay, detached pane, the settings window blocking the main window |
| [settings-window](settings-window.md) | 08 | Performance, Appearance, Storage, Input map, Diagnostics, About |
| [stacking-parameters](stacking-parameters.md) | 09, 14 | the dock's Settings tab: sections, values, algorithm picker, Reset All, per-tab values, brightness target |
| [images-panel](images-panel.md) | 10, 13 | frame list, exclude, remove, sort, reorder, add, List / Image list / Large previews |
| [projects](projects.md) | 11, 16, 18 | projects overview, cards, cancelled run, close from card, rail, overview opened at launch, held paused transport |
| [open-and-refusals](open-and-refusals.md) | 12, 16 | RAW folder, compressed TIFF variants, empty start, not-a-project, mismatch and missing-frames projects, grouping question, skipped subfolder, failed fuse |
| [notifications](notifications.md) | 15 | bell and unread count, notification center, toasts, crash record |
| [step-calculator](step-calculator.md) | 21, 23 | calculator button and overlay, Lens and Objective optics, entered boxes, SUMMARY read-out, violation line, one overlay at a time, no project open |
| [analytics](analytics.md) | 22 | Analytics button and overlay, one row per frame (chip, name, bar, count), waiting caption, recount on tab switch, close and Escape, one overlay at a time, button greyed with no project |

Each feature file carries a `Book:` line after `Guides:`: the manual sections (`page.md#anchor` under `docs/`) that show the feature, each holding an image. `tools/check-manual-coverage.py` checks it.

## Human only

Races against a running fuse (pause, resume, cancel, queued tab, controls inert while running: journey 02 steps 17-25, journey 09 steps 2-5) are driven: `--slow-run N` holds the Nth run of the launch in its running state for `--slow-run-seconds` (default 150 s). Key presses, typed text, hover and clicks in a second window are driven steps (`Press`, `Type`, `Hover`, `--window N`). Native dialogs (Open folder, + add frames, Save choosers), wheel and pan, pixel judgements, window manager moves, spinner motion and what needs the window's own keyboard focus (toasts, `manual/15-toasts-and-crash-record.md`) are `(human only)` steps, or listed in the feature's Gotchas when no journey can reach them. The pictures a running fuse draws in the Composite pane (the aligned frame, the result growing tile by tile with its sharpness inset, the growth over a kept composite) pass in seconds and are `(human only)` steps (journey 20 steps 3, 14 and 22). The crash-record steps (`manual/15-toasts-and-crash-record.md`) stay human only too: they need a crash report seeded into the data folder before launch.

## Not covered yet

What has to exist before the rest can be tested, most valuable first.

1. A seeded settings folder for the Recent list, and a journey that opens `fixtures/projects/valid.ffp` (the file exists; the corrupt, mismatch and missing projects are covered by journey 16).
2. The export writing and refused states: they need a lone launch (`--export-dialog-preview`); saves and exports already land in the `--save-folder` destination.
3. A journey that presses the keyboard shortcuts (Ctrl+Z, Ctrl+N, Ctrl+O, Ctrl+S, Ctrl+W, Escape on the Projects overview): `Press` exists, only Escape on the refusal, in the notification center and in the modal dialogs (journeys 05 and 06) is pressed so far.
4. Fixture folders: a stack that aligns fully, one large enough to show the loading indicator.
5. A driver drag that starts at a chosen point: `drag` and `hover` press at the element's centre.
6. A launch option that holds a run in its alignment stage or after some fused tiles: `--slow-run` holds before the first fuse tile, so no step can read the aligned frame or the growing result, and the "Sharpness heatmap" off check (no inset while fusing) waits on it.
