# Open with the Projects overview and a paused transport: the overview stays through the load, the paused transport stays through every switch
Start: --open fixtures/mantis-512/frames --open fixtures/mantis-64/frames --projects-overlay --toolbar-state paused
## The overview opened at launch stays up once both folders have loaded
1. Hover the first card (`FFProjectCard::card-area` 0).
   Expect: (within 60 s) the overview (`AppWindow::projects-overlay`) covers the window below the tab strip with the heading "Open projects" and the count "2 open"; the first card's second line reads "8 frames · no checkpoint yet" and the second card's reads "3 frames · no checkpoint yet"; both cards carry an "UNSAVED" chip and only the second carries a "CURRENT" chip; Must not: the toolbar, the dock or the bottom bar is visible.
2. Click **Save project** (`FFProjectsRail::save-project`).
   Expect: Must not: the overview closes or a file chooser opens; the overview is still on screen and **Save project** is greyed out (read with `buttons AppWindow::projects-overlay`: the line starts with `x`).
3. Press Escape.
   Expect: the overview is gone; the toolbar shows **Resume** and **Cancel** and no **Fuse stack** button; the bottom bar reads "Paused" and "ETA —"; the dock header reads "3 frames · 0 excluded".
## The paused transport belongs to no run, so its buttons do nothing
4. Click **Resume** (`FFToolbar::resume-button`).
   Expect: Must not: a run starts; the toolbar still shows **Resume** and **Cancel**, and the bottom bar still reads "Paused" with "ETA —".
5. Click **Cancel** (`FFToolbar::cancel-button`).
   Expect: Must not: a run is cancelled; the toolbar shows no "Cancelling…" button and still shows **Resume** and **Cancel**, and the bottom bar still reads "Paused" with "ETA —".
## The paused transport stays through tab, dock and zoom changes
6. Click **Settings** (`FFDockTabs::settings-tab`).
   Expect: the dock lists the headings "PREPARE", "FUSE", "ALIGN" and "FINISH"; the toolbar still shows **Resume** and **Cancel**; the bottom bar still reads "Paused" with "ETA —".
7. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the dock header reads "3 frames · 0 excluded"; the bottom bar still reads "Paused" with "ETA —".
8. Click the first tab (`FFTab::area` 0).
   Expect: the dock header reads "8 frames · 0 excluded"; the toolbar shows **Resume** and **Cancel** and no **Fuse stack** button; the bottom bar reads "Paused" with "ETA —".
9. Click **Zoom in** (`FFZoomCluster::zoom-in`).
   Expect: the zoom readout (`FFZoomCluster::zoom-readout`) reads a larger percentage than the one shown in step 8; the bottom bar still reads "Paused" with "ETA —".
10. Click the second tab (`FFTab::area` 1).
    Expect: the dock header reads "3 frames · 0 excluded"; the toolbar still shows **Resume** and **Cancel**; the bottom bar still reads "Paused" with "ETA —".
## The overview opened by hand closes on a tab choice
11. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview opens with the count "2 open"; the "CURRENT" chip is on the "3 frames" card and not on the "8 frames" card.
12. Click the first card (`FFProjectCard::card-area` 0).
    Expect: the overview is gone; the dock header reads "8 frames · 0 excluded"; the toolbar shows **Resume** and **Cancel**; the bottom bar reads "Paused" with "ETA —".
13. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview opens; the "CURRENT" chip is on the "8 frames" card and not on the "3 frames" card.
14. Click **Projects** (`FFTabStrip::projects-button`) again.
    Expect: the overview is gone; the dock header still reads "8 frames · 0 excluded"; the bottom bar reads "Paused" with "ETA —".
15. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview opens again with the count "2 open".
16. Press Escape.
    Expect: the overview is gone; the toolbar shows **Resume** and **Cancel**; the bottom bar reads "Paused" with "ETA —".
## Closing a project keeps the paused transport
17. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview opens with the count "2 open".
18. Click **Close project** (`FFProjectsRail::close-project`).
    Expect: no "Save this state first?" question appears; the overview stays open and its count reads "1 open" with one card, "3 frames · no checkpoint yet".
19. Press Escape.
    Expect: the overview is gone; the tab strip shows one tab; the dock header reads "3 frames · 0 excluded"; the toolbar shows **Resume** and **Cancel**; the bottom bar reads "Paused" with "ETA —".
20. Click **Resume** (`FFToolbar::resume-button`).
    Expect: Must not: a run starts; the toolbar still shows **Resume** and **Cancel**, and the bottom bar still reads "Paused" with "ETA —".
