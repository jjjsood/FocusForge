# Read the projects overview while working in two projects
Start: --open fixtures/mantis-64/frames --open fixtures/mantis-512/frames
## The gallery with two fresh projects
1. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overlay (`AppWindow::projects-overlay`) covers the window below the tab strip, with the left rail entries "New project", "Open project…", "Save project", "Close project" and "RECENT" (no row under it), the gallery heading "Open projects" and the count "2 open"; the first card's second line reads "3 frames · no checkpoint yet", the second card's reads "8 frames · no checkpoint yet", both carry an "UNSAVED" chip, and no card is headed "Cancelled".
## Switch between the two projects
2. Click the first card (`FFProjectCard::card-area` 0).
   Expect: the overlay is gone and the dock header reads "3 frames · 0 excluded".
3. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the "CURRENT" chip is on the "3 frames" card and not on the "8 frames" card.
4. Click the second card (`FFProjectCard::card-area` 1).
   Expect: the overlay is gone and the dock header reads "8 frames · 0 excluded".
5. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the "CURRENT" chip is now on the "8 frames" card and not on the "3 frames" card.
6. Click **Projects** (`FFTabStrip::projects-button`) again.
   Expect: the overlay is gone and the dock header still reads "8 frames · 0 excluded".
7. Click the first tab (`FFTab::area` 0).
   Expect: the dock header reads "3 frames · 0 excluded" and the tab strip shows two tabs titled "frames".
## Fuse the 3-frame project and read its unsaved card
8. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) a dialog titled "Keep this fused result?" is on screen.
9. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone and the bottom bar reads "fused" followed by a time.
10. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the first card's second line reads "3 frames · 1 checkpoint" and the card carries an "UNSAVED" chip and a "CURRENT" chip; the second card still reads "8 frames · no checkpoint yet" and has no "CURRENT" chip.
11. Click **Projects** (`FFTabStrip::projects-button`) again.
    Expect: the overlay is gone and the tab strip shows two tabs titled "frames", each with an "UNSAVED" marker.
12. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the rail shows "New project" with "Ctrl+N", "Open project…" with "Ctrl+O", "Save project" with "Ctrl+S" and "Close project" with "Ctrl+W", and the count reads "2 open".
## Close projects with and without unsaved work
13. Click the close glyph of the first tab (`FFTab::close-area` 0).
    Expect: Must not: the project closes at once; a dialog titled "Save this state first?" shows the line "Closing frames" and the text "1 unsaved checkpoint.", and the count behind it still reads "2 open".
14. Click **Cancel** (`AppWindow::close-guard`).
    Expect: the dialog is gone and the count still reads "2 open".
15. Click the close glyph of the second tab (`FFTab::close-area` 1).
    Expect: Must not: a dialog appears; the project closes at once, the count reads "1 open" and only the card "3 frames · 1 checkpoint" is left, carrying the "CURRENT" chip.
## Save, close and reopen from RECENT
16. Click **Save project** (`FFProjectsRail::save-project`).
    Expect: (within 30 s) the card's second line still reads "3 frames · 1 checkpoint", it carries the "CURRENT" chip and the "UNSAVED" chip is gone; no file chooser opens.
17. Click **Close project** (`FFProjectsRail::close-project`).
    Expect: Must not: a dialog appears; the overlay stays open, the count reads "0 open", the text "No projects open" is shown and one row "frames" sits under "RECENT" in the rail.
18. Click the RECENT row "frames" (`FFProjectsRail::recent-row` 0).
    Expect: (within 30 s) the overlay is gone and the tab strip shows one tab titled "frames" with no "UNSAVED" marker.
19. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the count reads "1 open" and the one card reads "3 frames · 1 checkpoint" with the "CURRENT" chip and no "UNSAVED" chip.
20. Hover the remaining card (`FFProjectCard::card-area` 0).
    Expect: `buttons AppWindow::projects-overlay` lists **Close** with id `FFProjectCard::close`; no dialog is shown and the count reads "1 open".
21. Click **Close** (`FFProjectCard::close`).
    Expect: Must not: a dialog appears; the project closes at once, the count reads "0 open" and the text "No projects open" is shown with no card listed.
## Rail actions and actions with nothing open
22. Click **Close project** (`FFProjectsRail::close-project`).
    Expect: Must not: a dialog or an error appears; the overlay stays open, the count reads "0 open" and the text "No projects open" is shown.
23. (human only) Click **New project** (`FFProjectsRail::new-project`), then cancel the folder chooser that opens.
    Expect: the overlay stays open and still shows "No projects open".
24. (human only) Click **Open project…** (`FFProjectsRail::open-project`), then cancel the file chooser that opens.
    Expect: the overlay stays open and still shows "No projects open".
25. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overlay is gone; the canvas shows the title "No project open" and a button "Open folder…".
26. Click **Fuse stack** (`FFToolbar::fuse-button`) with no project open.
    Expect: Must not: a job starts; the bottom bar status reads "No project open" and **Fuse stack** is greyed out (read with `buttons AppWindow::toolbar`: its line starts with `x`).
27. Click **Export image** (`FFCheckpointPanel::checkpoint-export`) with no project open.
    Expect: Must not: an "Export image" dialog opens; **Export image** is greyed out (read with `buttons FFDock::checkpoint-panel`: its line starts with `x`).
28. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the gallery heading reads "Open projects" with the count "0 open", the centred text "No projects open" is shown, no card is listed and one row "frames" sits under "RECENT" in the rail.
