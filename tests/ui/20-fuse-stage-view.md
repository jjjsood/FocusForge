# Watch a fuse in the Composite pane, then cancel, discard, keep and fuse again
Start: --open fixtures/mantis-512/frames --open fixtures/mantis-64/frames --slow-run 1 --slow-run-seconds 600
## Before any run
1. Click the first tab (`FFTab::area` 0).
   Expect: the dock header reads "8 frames · 0 excluded", the right pane shows the card "Not fused yet" with the line "Fuse stack builds the composite here.", the CHECKPOINTS list shows "None yet — fuse a stack" and the bottom bar status reads "No job running".
## A held fuse
2. Click **Fuse stack** (`FFToolbar::fuse-button`); this is the first run of the launch, which `--slow-run 1` holds in its fuse stage for up to 600 s of running time.
   Expect: (within 60 s) the bottom bar status reads "Fusing" and the right pane shows the card "Fusing" with the line "The result builds here tile by tile."; the card "Not fused yet" is gone.
3. (human only) Watch the right pane in the first seconds after the click of step 2 (only the first run of a launch aligns its frames). It reads "Preparing frames" with the line "Images appear here once alignment starts.", then the pane label reads "Aligned · " followed by a file name ending ".tif" over a frame lined up with the left pane, then the card "Fusing" stays. These seconds pass faster than a step can read them.
   Expect: the card "Preparing frames" shows first, the label "Aligned · <name>.tif" with a picture shows next and the card "Fusing" shows last.
4. Hover the progress bar (`FFBottomBar::progress-bar`).
   Expect: Must not: a separate bar or row of stage names (Preparing, Aligning, Fusing, Writing) shows above, between or below the panes; the bottom bar still shows the status "Fusing", the text "estimating" and its progress bar track.
5. Click **Pause** (`FFToolbar::pause-button`).
   Expect: the bottom bar status reads "Paused", the toolbar shows **Resume** and the right pane still shows the card "Fusing".
6. Click **Resume** (`FFToolbar::resume-button`).
   Expect: the bottom bar status reads "Fusing", the toolbar shows **Pause** and the right pane still shows the card "Fusing".
7. Click the second tab (`FFTab::area` 1) while the first tab's run is held.
   Expect: Must not: the card "Fusing" shows in the right pane; it shows the card "Not fused yet", the dock header reads "3 frames · 0 excluded" and the toolbar shows a single button reading "Queued".
8. Click the first tab (`FFTab::area` 0).
   Expect: the dock header reads "8 frames · 0 excluded" and the right pane shows the card "Fusing" again.
9. Click the undock control of the Composite pane (`FFUndockControl::area` 1).
   Expect: (within 5 s) the main window shows one pane, labelled "00.tif", with no card on it; the new window (`$M shot <png> N --scale 0.5` with N the last entry of `list_windows`, counted from 0) has the title bar "FocusForge — Composite" and shows the card "Fusing".
10. Click the dock control (label "Dock pane", `FFUndockControl::area` 0) in the second window (`--window N`, N the last entry of `list_windows`, counted from 0).
    Expect: the second window is gone and the right pane of the main window shows the card "Fusing" again.
## A cancelled run
11. Click **Cancel** (`FFToolbar::cancel-button`).
    Expect: (within 30 s) the toolbar shows a **Fuse stack** button again, the right pane shows the card "Not fused yet" and no card "Fusing", and the bottom bar status reads "No job running".
12. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: Must not: the CHECKPOINTS list holds a row; it still shows the line "None yet — fuse a stack".
## A fuse that is discarded
13. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep".
14. (human only) Watch the right pane in the first seconds after the click of step 13. The card reads "Alignment reused" with the line "Frames were aligned in an earlier run. Fusing starts next." and no "Aligned · " label shows; then the result grows tile by tile with a grid on the tiles not fused yet, the tiles just fused framed in the accent colour, and a card "Sharpness" at the bottom right with the words "blurred" and "sharp".
    Expect: the card "Alignment reused" shows first, then the growing result with its grid and its "Sharpness" card.
15. Click **Discard** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone, the right pane shows the card "Not fused yet" with no picture and no "Sharpness" card, and the bottom bar status reads "No job running" (within 1 s).
16. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: Must not: the CHECKPOINTS list holds a row; it still shows the line "None yet — fuse a stack".
## A fuse that is kept
17. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) the dialog "Keep this fused result?" with "Discard" and "Keep" is shown.
18. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bottom bar status reads "fused" followed by a time (within 1 s).
19. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the CHECKPOINTS list holds one row, the right pane is labelled "Composite" and shows a filled image, and it shows no card "Not fused yet", no card "Fusing" and no "Sharpness" card (within 1 s).
## Fuse again over the kept result
20. Click **Fuse stack** (`FFToolbar::fuse-button`) with the kept result on screen.
    Expect: Must not: the right pane shows a card "Not fused yet", "Alignment reused", "Preparing frames" or "Fusing"; it shows image content (the run ends within seconds, so the question may already cover it).
21. Hover the progress bar (`FFBottomBar::progress-bar`) until the run ends in its question.
    Expect: (within 60 s) the dialog "Keep this fused result?" with "Discard" and "Keep" is shown.
22. (human only) Watch the right pane in the seconds after the click of step 20. The result grows tile by tile over the kept composite, with its grid and its "Sharpness" card, and the kept composite is hidden under it.
    Expect: the growing result covers the kept composite tile by tile.
23. Click **Discard** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone, the right pane still shows a filled composite and no card, and the CHECKPOINTS list holds one row (within 1 s).
