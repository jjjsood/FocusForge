# Open folders of BigTIFF frames, read the refusal, then fuse a readable 8-bit stack
Start: --open fixtures/mantis8-64 --open .scratch/bigtiff-gui/one-bigtiff --open .scratch/bigtiff-gui/all-bigtiff
Before the first step, build the two refused folders. A BigTIFF file starts with the 8-byte header below: `II` or `MM`, the number 43, then the offset size 8. No picture is needed, the header alone is refused:
`mkdir -p .scratch/bigtiff-gui/all-bigtiff .scratch/bigtiff-gui/one-bigtiff`
`printf 'II\x2b\x00\x08\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00' > .scratch/bigtiff-gui/all-bigtiff/00.tif`
`printf 'MM\x00\x2b\x00\x08\x00\x00\x00\x00\x00\x00\x00\x00\x00\x10' > .scratch/bigtiff-gui/all-bigtiff/01.tif`
`cp .scratch/bigtiff-gui/all-bigtiff/00.tif .scratch/bigtiff-gui/all-bigtiff/02.tif`
`cp fixtures/mantis-64/frames/00.tif fixtures/mantis-64/frames/01.tif .scratch/bigtiff-gui/one-bigtiff/`
`cp .scratch/bigtiff-gui/all-bigtiff/00.tif .scratch/bigtiff-gui/one-bigtiff/02.tif`
The window opens the three inputs in order: the 8-bit stack becomes a project, and each BigTIFF folder is refused. Only the last refusal stays on screen; the first one is replaced by it.
## The BigTIFF folders are refused where they are opened
1. Click **Projects** (`FFTabStrip::projects-button`) while the launch's refusal is up.
   Expect: (within 30 s) Must not: the Projects overview opens; a dialog titled "These frames cannot be read" with the text "00.tif is a BigTIFF file, which FocusForge cannot read. Save the frames as standard TIFF and open them again." and a **Close** button stays on top; the text holds no folder path.
2. Click **Fuse stack** (`FFToolbar::fuse-button`) while the dialog is up.
   Expect: Must not: a fuse starts; the dialog stays on screen and the bottom bar status still reads "No job running".
3. Click **Close** within the refusal (`click "Close" AppWindow::refusal-dialog`).
   Expect: the dialog is gone and the tab strip shows one tab, "mantis8-64", and no tab for either BigTIFF folder.
4. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overview shows the count "3 · 2 did not open"; the card "mantis8-64" reads "3 frames · no checkpoint yet" and carries the "CURRENT" chip; two cards are headed "Not a project", named "one-bigtiff" and "all-bigtiff", each with the line "not read · not changed".
5. Click the card "one-bigtiff" (`FFProjectCard::card-area` 1).
   Expect: Must not: a tab opens or the overview closes; the count still reads "3 · 2 did not open" and the tab strip shows one tab.
6. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overview is gone and the toolbar shows **Fuse stack** (`FFToolbar::fuse-button`).
## The readable 8-bit folder shows its frames
7. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the dock header reads "3 frames · 0 excluded" and the list shows 3 rows named "00.tif", "01.tif" and "02.tif", each with a small picture; no row reads "Preview unavailable".
8. Click the second frame row (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif" and the pane shows a picture; the text "Cannot show this frame" is nowhere on screen.
9. Click the third frame row (`FFImageRow::area` 2).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "02.tif" and the pane shows a picture; the text "Cannot show this frame" is nowhere on screen.
10. Click the first frame row (`FFImageRow::area` 0).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "00.tif" and the pane shows a picture.
## The readable stack fuses
11. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) a dialog titled "Keep this fused result?" with the buttons "Discard" and "Keep" is on screen.
12. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone and the bottom bar reads "fused" followed by a time.
13. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the right pane (`FFCanvas::pane-right`) shows a filled composite and the CHECKPOINTS list holds 1 row.
14. Click **Notifications** (`FFIconBar::bell`).
   Expect: Must not: a row titled "Setup failed" is listed; the center reads "1 notification · this session" with one row titled "2 frames unaligned".
15. Click **Notifications** (`FFIconBar::bell`).
   Expect: the center is gone.
## Close the project and reach the empty start
16. Click the close glyph of the only tab (`FFTab::close-area` 0).
   Expect: a dialog titled "Save this state first?" shows the line "Closing mantis8-64" and the text "1 unsaved checkpoint."
17. Click **Close anyway** (`AppWindow::close-guard`).
   Expect: the dialog is gone, the tab strip shows no tab and the canvas shows the title "No project open" (`FFEmptyState::empty-title`).
18. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overview shows the heading "Projects" and the count "2 · 2 did not open"; the two cards are headed "Not a project" and named "one-bigtiff" and "all-bigtiff"; no card shows a frame count.
19. Click the card "all-bigtiff" (`FFProjectCard::card-area` 1).
   Expect: Must not: a tab opens or the overview closes; the count still reads "2 · 2 did not open" and the tab strip shows no tab.
20. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overview is gone and the canvas shows the title "No project open" and a filled **Open…** button (`FFEmptyState::open`).
21. Click **Fuse stack** (`FFToolbar::fuse-button`) with no project open.
   Expect: Must not: a fuse starts or a dialog opens; **Fuse stack** is greyed out (read with `buttons AppWindow::toolbar`: its line starts with `x`) and the bottom bar status reads "No project open".
