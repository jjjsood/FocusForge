# Open a camera RAW folder and a compressed stack, then reach the empty start
Start: --open fixtures/blackmagic-micro-dng --open fixtures/mantis-64-lzw-p2-tile-mm
## Two folders, two projects
1. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overlay (`AppWindow::projects-overlay`) shows two cards and the count "2 open"; the first card is named "frame" with the second line "1 frame · no checkpoint yet", the second card is named "mantis-64-lzw-p2-tile-mm" with "3 frames · no checkpoint yet"; the "CURRENT" chip is on the second card and not on the first.
## A camera RAW folder opens as a one-frame project
2. Click the first card, "frame" (`FFProjectCard::card-area` 0).
   Expect: the overlay is gone, the tab strip shows two tabs and no dialog "No frames here" is on screen; the left pane label (`FFCanvas::pane-left`) reads "frame.dng".
3. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the dock header reads "1 frame · 0 excluded" and the list shows 1 row, "frame.dng".
4. Click the first frame row (`FFImageRow::area` 0).
   Expect: the left pane label still reads "frame.dng" and the list still shows 1 row.
5. Click **Fuse stack** (`FFToolbar::fuse-button`) while only this one frame is open.
   Expect: Must not: a fuse starts or a dialog opens; the bottom bar status still reads "No job running" and **Fuse stack** is greyed out (read with `buttons AppWindow::toolbar`: its line starts with `x`).
## A compressed-TIFF folder fuses like the plain stack
6. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the "CURRENT" chip is on the first card, "frame", and not on the second card.
7. Click the second card (`FFProjectCard::card-area` 1).
   Expect: the overlay is gone and the dock header reads "3 frames · 0 excluded".
8. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the list shows 3 rows.
9. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) a dialog titled "Keep this fused result?" is on screen.
10. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bottom bar reads "fused" followed by a time.
11. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the right pane shows a filled composite and the CHECKPOINTS list holds one row.
## Close the projects and reach the empty start
12. Click the close glyph of the second tab (`FFTab::close-area` 1).
    Expect: a dialog titled "Save this state first?" shows the line "Closing mantis-64-lzw-p2-tile-mm" and the text "1 unsaved checkpoint."
13. Click **Close anyway** (`AppWindow::close-guard`).
    Expect: the dialog is gone and the tab strip shows one tab.
14. Click the close glyph of the first tab (`FFTab::close-area` 0).
    Expect: Must not: a dialog appears; the tab strip shows no tab and the canvas shows the title "No project open" (`FFEmptyState::empty-title`) and a button "Open folder…".
15. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overlay shows the gallery heading "Open projects" with the count "0 open", the centred text "No projects open" and no card.
16. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overlay is gone and the canvas shows the text "Open a folder of frames. Each detected stack becomes its own project, in its own tab." and a filled **Open folder…** button; no pane labelled "Composite" is visible.
17. Click **Zoom in** (`FFZoomCluster::zoom-in`).
    Expect: the zoom readout (`FFZoomCluster::zoom-readout`) reads "—" and the bottom bar status text reads "No project open".
18. Click **Fit** (`FFZoomCluster::zoom-fit`).
    Expect: the zoom readout still reads "—" and the canvas still shows the title "No project open".
19. Click the **Overlay** toggle (`FFBottomBar::overlay-toggle`).
    Expect: Must not: the toggle turns on; it stays off and dimmed, and the right dock reads "No project open." with no **Images** or **Settings** tabs.
20. (human only) Click **Open folder…** (`FFEmptyState::open-folder`), then cancel the folder chooser that opens.
    Expect: a native folder chooser opens; after cancelling it the canvas still shows the title "No project open".
