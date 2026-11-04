# Analytics
Reach: press the Analytics button in the icon bar on the left edge, directly under the step calculator button (label **Analytics**, inside `AppWindow::icon-bar`). It is greyed out while no project is selected. The same button, **Close** (top right of the overlay) and Escape close it. `--open <folder> --analytics` opens it once the folder has loaded; `--analytics-hold-ms MS` holds each count back so the waiting state can be looked at.
Sub-features: one row per frame of the selected tab in the dock's order (chip, file name, bar, count; excluded frames are listed too); bar length is the count divided by the largest count, empty for every row when all counts are 0; no millimetre column without rail positions; the waiting caption "Counting sharp areas..." while the count runs; the count restarts for the newly selected tab; closing the overlay stops the count; one full-canvas overlay at a time (the projects overview, the notification center and the step calculator close it, and it closes them).
Ids: `AppWindow::icon-bar` (the button is clicked by its label), `AppWindow::analytics-overlay` (**Close** is clicked by label inside it).
Gotchas:
- The overlay covers the canvas only: the toolbar, the dock and the bottom bar stay visible. The window's title-bar button is also labelled "Close", so a Close step names `AppWindow::analytics-overlay`.
- Counts of `fixtures/mantis-512/frames` (00.tif to 07.tif): 0, 0, 4, 11, 0, 0, 0, 1. `fixtures/mantis-64/frames` counts 0 for all three frames.
- With the hold flag every opening shows the waiting caption for the hold time; a step that reads rows says `(within 60 s)`.
- Not covered by the journey: the failed state (a frame deleted after the folder was opened, reachable only by deleting a file during the hold), a list taller than the body with its scrollbar (needs a stack of more frames than fits, none is checked in, and the wheel is a human-only input), and the editor mode. Check them by hand with a copy of a fixture under `.scratch/`.
- Opening the overlay with a driver needs a build with the `mcp` feature (`tools/gui/run-gui.sh` builds it); a binary built without it answers no request.
Guides: 22-analytics.md
