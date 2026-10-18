# Fuse from launch, keep or discard it per tab, then pause, queue and cancel by hand
Start: --open fixtures/mantis-512/frames --open fixtures/mantis-64/frames --fuse-tabs 0 --slow-run 4
## The fuse that started at launch
1. Click the first tab (`FFTab::area` 0).
   Expect: (within 60 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep", and the text "00.tif, 01.tif, 06.tif and 07.tif could not be aligned", although **Fuse stack** was never clicked; the dock header behind it reads "8 frames · 0 excluded".
2. Click the second tab (`FFTab::area` 1) while the dialog is open.
   Expect: Must not: the second tab opens; the dialog is still on screen and the dock header behind it still reads "8 frames · 0 excluded".
3. Click **Projects** (`FFTabStrip::projects-button`) while the dialog is open.
   Expect: Must not: the projects overview opens; the dialog is still on screen and the text "Open projects" is not shown.
4. Click **Discard** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone, the bottom bar status reads "No job running" and the toolbar shows a **Fuse stack** button (read with `buttons AppWindow::toolbar`).
5. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: Must not: the CHECKPOINTS list holds a row; it shows the line "None yet — fuse a stack" and the right pane shows the card "Not fused yet".
## The second tab was never fused
6. Click the second tab (`FFTab::area` 1).
   Expect: the dock header reads "3 frames · 0 excluded", the right pane shows the card "Not fused yet" and the toolbar shows only a **Fuse stack** button, no **Pause**, **Cancel** or "Queued" button (read with `buttons AppWindow::toolbar`).
## Fuse the first tab again and keep it
7. Click the first tab (`FFTab::area` 0).
   Expect: the dock header reads "8 frames · 0 excluded" and the toolbar shows a **Fuse stack** button (read with `buttons AppWindow::toolbar`).
8. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) the dialog "Keep this fused result?" with "Discard" and "Keep" is shown again.
9. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone and the bottom bar status reads "fused" followed by a time.
10. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the CHECKPOINTS list holds one row and the right pane shows a filled composite without the card "Not fused yet".
## A result belongs to its own tab
11. Click the second tab (`FFTab::area` 1).
    Expect: Must not: the CHECKPOINTS list holds a row; it shows the line "None yet — fuse a stack" and the right pane shows the card "Not fused yet".
12. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) the dialog "Keep this fused result?" shows the text "00.tif and 02.tif could not be aligned".
13. Click **Discard** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the CHECKPOINTS list still shows the line "None yet — fuse a stack".
14. Click the first tab (`FFTab::area` 0).
    Expect: the CHECKPOINTS list holds one row and the dock header reads "8 frames · 0 excluded".
15. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the first card's second line reads "8 frames · 1 checkpoint" and the card carries the "CURRENT" chip; the second card's second line reads "3 frames · no checkpoint yet"; no card is headed "Cancelled".
16. Click **Projects** (`FFTabStrip::projects-button`) again.
    Expect: the overlay is gone and the toolbar shows a **Fuse stack** button (read with `buttons AppWindow::toolbar`).
## Pause, queue and cancel a running fuse
17. Click **Fuse stack** (`FFToolbar::fuse-button`) on the first tab; this is the fourth run of the launch, which `--slow-run 4` holds in its fuse stage for at least 150 s.
    Expect: (within 60 s) the bottom bar status reads "Fusing" and the toolbar shows **Pause** and **Cancel** and no **Fuse stack** button (the grey text at the right end of the toolbar is not a button).
18. Click **Pause** (`FFToolbar::pause-button`) on the running first tab.
    Expect: the toolbar shows **Resume** (`FFToolbar::resume-button`) instead of **Pause**, and the bottom bar status reads "Paused".
19. Click the second tab (`FFTab::area` 1) while the first tab's run is paused.
    Expect: the toolbar shows a single button reading "Queued" (`FFToolbar::fuse-button`), because the first tab is running.
20. Click the first tab (`FFTab::area` 0).
    Expect: the toolbar shows **Resume** and **Cancel**, not **Pause**, and the bottom bar status reads "Paused".
21. Click **Cancel** (`FFToolbar::cancel-button`) on the paused run.
    Expect: (within 30 s) the **Fuse stack** button is back in the toolbar, and **Pause**, **Resume** and **Cancel** are gone (a greyed "Cancelling…" button may show for a moment first).
22. Click **Projects** (`FFTabStrip::projects-button`) after the run was cancelled.
    Expect: the first card is headed "Cancelled", its stage line starts "STOPPED WHILE", its text reads "You stopped this one. Nothing was written.", its second line reads "8 frames · 1 checkpoint", and it shows a button "Run again".
23. Click **Run again** (`AppWindow::projects-overlay`) on the cancelled card.
    Expect: (within 60 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep"; behind it the first card still reads "Cancelled".
24. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone; the first card no longer reads "Cancelled" and the text "Run again" is gone from it; its second line starts with "8 frames · ".
25. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overlay is gone and the text "Open projects" is not shown.
