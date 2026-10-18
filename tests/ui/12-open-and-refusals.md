# Open a camera RAW folder and a compressed stack, then reach the empty start
Start: --open fixtures/blackmagic-micro-dng --open fixtures/mantis-64-lzw-p2-tile-mm --open fixtures/retouch --slow-open 20
## While the folders load
1. Click **Zoom in** (`FFZoomCluster::zoom-in`) within a few seconds of launch, while the first folder is still loading.
   Expect: the canvas shows the text "Loading…" with a spinner and the zoom readout (`FFZoomCluster::zoom-readout`) reads "—".
## Two folders, two projects
2. Click **Fit** (`FFZoomCluster::zoom-fit`) and wait for the third folder, which holds no frames, to be read.
   Expect: (within 90 s) the tab strip shows two tabs, the canvas no longer shows "Loading…" and a dialog titled "No frames here" with the button "Pick another folder…" is on screen.
## The refusal is dismissed with a key
3. Press Escape.
   Expect: the dialog "No frames here" is gone, no file chooser is on screen and the tab strip still shows two tabs.
4. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overlay (`AppWindow::projects-overlay`) shows three cards and the count "3 · 1 did not open"; the first card is named "frame" with the second line "1 frame · no checkpoint yet", the second card is named "mantis-64-lzw-p2-tile-mm" with "3 frames · no checkpoint yet", the third is headed "Not a project" and named "retouch" with the line "not read · not changed"; the "CURRENT" chip is on the second card and not on the first.
## A camera RAW folder opens as a one-frame project
5. Click the first card, "frame" (`FFProjectCard::card-area` 0).
   Expect: the overlay is gone, the tab strip shows two tabs and no dialog "No frames here" is on screen; the left pane label (`FFCanvas::pane-left`) reads "frame.dng".
6. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the dock header reads "1 frame · 0 excluded" and the list shows 1 row, "frame.dng".
7. Click the first frame row (`FFImageRow::area` 0).
   Expect: the left pane label still reads "frame.dng" and the list still shows 1 row.
8. Click **Fuse stack** (`FFToolbar::fuse-button`) while only this one frame is open.
   Expect: Must not: a fuse starts or a dialog opens; the bottom bar status still reads "No job running" and **Fuse stack** is greyed out (read with `buttons AppWindow::toolbar`: its line starts with `x`).
## A compressed-TIFF folder fuses like the plain stack
9. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the "CURRENT" chip is on the first card, "frame", and not on the second card.
10. Click the second card (`FFProjectCard::card-area` 1).
   Expect: the overlay is gone and the dock header reads "3 frames · 0 excluded".
11. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the list shows 3 rows.
12. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) a dialog titled "Keep this fused result?" is on screen.
13. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bottom bar reads "fused" followed by a time.
14. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the right pane shows a filled composite and the CHECKPOINTS list holds one row.
## Close the projects and reach the empty start
15. Click the close glyph of the second tab (`FFTab::close-area` 1).
    Expect: a dialog titled "Save this state first?" shows the line "Closing mantis-64-lzw-p2-tile-mm" and the text "1 unsaved checkpoint."
16. Click **Close anyway** (`AppWindow::close-guard`).
    Expect: the dialog is gone and the tab strip shows one tab.
17. Click the close glyph of the first tab (`FFTab::close-area` 0).
    Expect: Must not: a dialog appears; the tab strip shows no tab and the canvas shows the title "No project open" (`FFEmptyState::empty-title`) and a button "Open folder…".
18. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overlay shows the gallery heading "Projects" with the count "1 · 1 did not open" and one card, headed "Not a project" and named "retouch"; no card shows a frame count.
19. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overlay is gone and the canvas shows the text "Open a folder of frames. Each detected stack becomes its own project, in its own tab." and a filled **Open folder…** button; no pane labelled "Composite" is visible.
20. Click **Zoom in** (`FFZoomCluster::zoom-in`).
    Expect: the zoom readout (`FFZoomCluster::zoom-readout`) reads "—" and the bottom bar status text reads "No project open".
21. Click **Fit** (`FFZoomCluster::zoom-fit`).
    Expect: the zoom readout still reads "—" and the canvas still shows the title "No project open".
22. Click the **Overlay** toggle (`FFBottomBar::overlay-toggle`).
    Expect: Must not: the toggle turns on; it stays off and dimmed, and the right dock reads "No project open." with no **Images** or **Settings** tabs.
23. (human only) Click **Open folder…** (`FFEmptyState::open-folder`), then cancel the folder chooser that opens.
    Expect: a native folder chooser opens; after cancelling it the canvas still shows the title "No project open".
