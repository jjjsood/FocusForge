# A queued tab runs while another tab waits at its Keep question
Start: --open fixtures/mantis-512/frames --open fixtures/mantis-64/frames --fuse-tabs 0,1 --slow-run 2 --slow-run-seconds 60
## The second tab fuses while the first waits
1. Click the second tab (`FFTab::area` 1), the tab the launch left selected.
   Expect: the bottom bar status reads either "Queued (2)" with only **Cancel** in the toolbar (the first tab's fuse is still running) or "Fusing" with **Pause** and **Cancel** (it has ended); either way no **Fuse stack** button (read with `buttons AppWindow::toolbar`) and the dock header reads "3 frames · 0 excluded"; Must not: a dialog titled "Keep this fused result?" is on screen.
2. Click the first tab (`FFTab::area` 0).
   Expect: (within 90 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep", and the text "00.tif and 01.tif could not be aligned"; the dock header behind it reads "8 frames · 0 excluded".
3. Click the second tab (`FFTab::area` 1) while the dialog is open.
   Expect: Must not: the second tab opens; the dialog is still on screen and the dock header behind it still reads "8 frames · 0 excluded".
4. Click **Projects** (`FFTabStrip::projects-button`) while the dialog is open.
   Expect: Must not: the projects overview opens; the dialog is still on screen and the text "Open projects" is not shown.
5. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone and the bottom bar status reads "fused" followed by a time (a toast about 2 unaligned frames may show for a moment).
6. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the CHECKPOINTS list holds one row and the right pane shows a filled composite without the card "Not fused yet".
## The second tab asks its own question
7. Click the second tab (`FFTab::area` 1).
   Expect: (within 90 s) a dialog titled "Keep this fused result?" shows the text "00.tif and 02.tif could not be aligned"; the dock header behind it reads "3 frames · 0 excluded".
8. Click the first tab (`FFTab::area` 0) while the dialog is open.
   Expect: Must not: the first tab opens; the dialog is still on screen and still shows "00.tif and 02.tif could not be aligned".
9. Click **Projects** (`FFTabStrip::projects-button`) while the dialog is open.
   Expect: Must not: the projects overview opens; the dialog is still on screen and the text "Open projects" is not shown.
10. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bottom bar status reads "fused" followed by a time.
11. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the CHECKPOINTS list holds one row and the right pane shows a filled composite without the card "Not fused yet".
## Each tab holds its own result
12. Click the first tab (`FFTab::area` 0).
    Expect: the CHECKPOINTS list holds one row, the dock header reads "8 frames · 0 excluded" and no dialog is shown.
13. Click the second tab (`FFTab::area` 1).
    Expect: the CHECKPOINTS list holds one row, the dock header reads "3 frames · 0 excluded" and no dialog is shown.
14. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the first card's second line reads "8 frames · 1 checkpoint"; the second card's second line reads "3 frames · 1 checkpoint" and the card carries the "CURRENT" chip; no card is headed "Cancelled".
15. Click **Projects** (`FFTabStrip::projects-button`) again.
    Expect: the overlay is gone and the toolbar shows a **Fuse stack** button (read with `buttons AppWindow::toolbar`).
## A new run on one tab never touches the other tab's question
16. Click **Fuse stack** (`FFToolbar::fuse-button`) on the second tab.
    Expect: (within 60 s) a dialog titled "Keep this fused result?" shows the text "00.tif and 02.tif could not be aligned".
17. Click **Discard** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone, the bottom bar status reads "No job running" and the CHECKPOINTS list still holds one row.
18. Click the first tab (`FFTab::area` 0).
    Expect: the CHECKPOINTS list holds one row, the dock header reads "8 frames · 0 excluded" and the toolbar shows a **Fuse stack** button (read with `buttons AppWindow::toolbar`).
19. Click **Fuse stack** (`FFToolbar::fuse-button`) on the first tab.
    Expect: (within 60 s) a dialog titled "Keep this fused result?" shows the text "00.tif and 01.tif could not be aligned".
20. Click **Discard** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone, the bottom bar status reads "No job running" and the CHECKPOINTS list still holds one row.
21. Click the second tab (`FFTab::area` 1).
    Expect: no dialog is shown, the CHECKPOINTS list holds one row and the toolbar shows a **Fuse stack** button (read with `buttons AppWindow::toolbar`).
22. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the first card's second line reads "8 frames · 1 checkpoint" and the second card's second line reads "3 frames · 1 checkpoint"; no card is headed "Cancelled".
23. Click **Projects** (`FFTabStrip::projects-button`) again.
    Expect: the overlay is gone and the text "Open projects" is not shown.
