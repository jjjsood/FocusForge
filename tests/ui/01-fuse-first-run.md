# Fuse a stack for the first time: refusals before, discard, keep, fuse again, fuse with alignment off
Start: --open fixtures/mantis-64/frames
## Before the first fuse
1. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the right pane shows a card titled "Not fused yet" with the line "Fuse stack builds the composite here."
2. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the bottom bar status reads "No job running", and the toolbar shows a **Fuse stack** button (read with `buttons AppWindow::toolbar`) and no **Pause** or **Cancel** button.
3. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: Must not: an "Export image" dialog opens; **Export image** is greyed out (read with `buttons FFDock::checkpoint-panel`: the line for `FFCheckpointPanel::checkpoint-export` starts with `x`) and the CHECKPOINTS list holds no row and shows the line "None yet — fuse a stack".
4. Click the **Overlay** toggle (`FFBottomBar::overlay-toggle`).
   Expect: Must not: the overlay turns on; the toggle stays off (knob on the left, no accent fill).
## Fuse and discard
5. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep", and its text contains "could not be aligned".
6. Click **Fuse stack** (`FFToolbar::fuse-button`) while the dialog is open.
   Expect: Must not: a second dialog appears or the first one closes; exactly one "Keep this fused result?" dialog with "Discard" and "Keep" is still shown.
7. Click **Discard** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone and the bottom bar status reads "No job running".
8. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: Must not: the CHECKPOINTS list holds a row; it shows the line "None yet — fuse a stack" and the right pane shows the card "Not fused yet".
9. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: Must not: an "Export image" dialog opens; **Export image** is still greyed out (read with `buttons FFDock::checkpoint-panel`: its line starts with `x`).
10. Click the **Overlay** toggle (`FFBottomBar::overlay-toggle`).
    Expect: Must not: the overlay turns on; the toggle stays off (knob on the left, no accent fill).
## Fuse again and keep
11. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) the dialog "Keep this fused result?" with "Discard" and "Keep" is shown again.
12. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bottom bar status reads "fused" followed by a time (for example "fused 16:34").
13. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the right pane shows a filled composite without the card "Not fused yet"; the CHECKPOINTS list holds one row and the line "None yet — fuse a stack" is gone.
14. Click the **Overlay** toggle (`FFBottomBar::overlay-toggle`).
    Expect: the toggle is on (knob on the right, accent fill).
## A second kept result and a second discard
15. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) the dialog "Keep this fused result?" with "Discard" and "Keep" is shown.
16. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bottom bar status reads "fused" followed by a time.
17. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the CHECKPOINTS list holds two rows.
18. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) the dialog "Keep this fused result?" with "Discard" and "Keep" is shown.
19. Click **Discard** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone.
20. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: Must not: the CHECKPOINTS list holds a third row; it still holds exactly two rows.
## Alignment off: the result is kept with no question
21. Click **Settings** (`FFDockTabs::settings-tab`).
    Expect: the Settings dock lists the headings "PREPARE", "FUSE" and "ALIGN" (FINISH sits below the fold), and the **Enabled** switch under ALIGN is on.
22. Click the **Enabled** switch under ALIGN (`FFToggle::area` 0).
    Expect: the switch is off and the ALIGN "Algorithm" label and field are drawn greyed out.
23. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) the bottom bar status reads "fused" followed by a time, and no dialog titled "Keep this fused result?" is on screen.
24. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the CHECKPOINTS list holds three rows.
