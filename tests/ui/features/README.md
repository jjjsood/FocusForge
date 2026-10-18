# Features

Each feature file says how a user reaches the feature, which controls matter, and which journeys cover it (one to three per feature). Every fixture fuse ends in the keep question ("Keep this fused result?"), because the fixture frames never align fully; journeys answer **Keep** as their own step. A kept fuse lands in the editor.

| Feature | Journeys | Covers |
|---|---|---|
| [fuse-stack](fuse-stack.md) | 01, 02 | fuse, keep or discard, not-fused state, pause, resume, cancel, queued tab, cancelled-run record |
| [retouch](retouch.md) | 03, 04 | editor mode, brush strokes, size and softness, undo and redo, source frame, save checkpoint, fuse-again and restore questions, select and delete checkpoints |
| [checkpoints-export](checkpoints-export.md) | 04, 05 | CHECKPOINTS list, the export dialog and its states |
| [close-and-quit](close-and-quit.md) | 06 | close-tab and quit questions, Quit without saving |
| [navigation-and-settings](navigation-and-settings.md) | 07 | dock tabs, zoom, two tabs, Z-Map overlay, detached pane, the settings window blocking the main window |
| [settings-window](settings-window.md) | 08 | Performance, Appearance, Storage, Input map, Diagnostics, About |
| [stacking-parameters](stacking-parameters.md) | 09, 14 | the dock's Settings tab: sections, values, algorithm picker, Reset All, per-tab values, brightness target |
| [images-panel](images-panel.md) | 10, 13 | frame list, exclude, remove, sort, reorder, add, List / Image list / Large previews |
| [projects](projects.md) | 11 | projects overview, cards, cancelled run, close from card, rail |
| [open-and-refusals](open-and-refusals.md) | 12 | RAW folder, compressed TIFF variants, empty start |
| [notifications](notifications.md) | 15 | bell and unread count, notification center, toasts, crash record |

## Human only

Races against a running fuse (pause, resume, cancel, queued tab, controls inert while running: journey 02 steps 17-25, journey 09 steps 2-5) are driven: `--slow-run N` holds the Nth run of the launch in its running state for `--slow-run-seconds` (default 150 s). Native dialogs (Open folder, + add frames, Save choosers), key presses, typing, wheel and pan, hover-only controls, pixel judgements, window manager moves and spinner motion are `(human only)` steps, or listed in the feature's Gotchas when no journey can reach them.

## Not covered yet

What has to exist before the rest can be tested, most valuable first.

1. A way to dismiss the "No frames here" refusal without the native chooser (it blocks the window at launch).
2. A key step in the driver (Ctrl+Z, Ctrl+N/O/S/W, Esc, Alt+click) and a text input step (parameter values, memory caps, Save to).
3. Project file fixtures under `fixtures/projects/` (valid, frames differ, frames missing, corrupt) and a seeded settings folder for the Recent list.
4. A save and export destination that is not the fixture folder; the export writing and refused states need a lone launch (`--export-dialog-preview`).
5. Element ids on the projects rail entries, the card close glyph and the checkpoint trash icon; a hover step in the driver.
6. Driver clicks in the second window (detached pane) and a drag that starts at a chosen point.
7. A slow fixture or a throttle that holds a fuse in the running state for minutes, so pause, resume, cancel and queued tabs can be read step by step.
8. Fixture folders: two stacks (grouping question), frames in the root plus a subfolder (skipped notice), a stack that aligns fully, one the pipeline rejects (Retry), one large enough to show the loading indicator.
