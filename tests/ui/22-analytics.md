# Count the sharp areas of every frame in the Analytics overlay
Start: --open fixtures/mantis-64/frames --open fixtures/mantis-512/frames --analytics --analytics-hold-ms 25000

Use a fresh settings directory and a fresh data directory. Two folders are open: the first tab holds 3 frames, the second (selected) holds 8 frames. The overlay starts open once both folders are loaded. Every count is held back for 25 seconds, so the rows stay pending (a sweeping bar and a spinner in place of the count, no number) until a count has finished once. The count follows the selected tab: it starts when a stack is loaded, a count that was left unfinished by opening another folder starts again when its tab is selected, and a finished count is kept and shown at once on every later opening. Steps that wait for the rows say `(within 60 s)`.

The Analytics button sits in the 48 px icon bar on the left edge, directly under the step calculator button, above the bell and Settings; it is reached by its label **Analytics** inside `AppWindow::icon-bar`, and it is never greyed out. The overlay (`AppWindow::analytics-overlay`) covers the canvas below the toolbar and above the bottom bar; the right dock stays visible. While it is open the button is drawn selected: an accent-coloured icon and a thin accent bar on the icon bar's left edge. The overlay is titled "Analytics", has the heading "SHARP AREAS PER IMAGE" with the subtitle "Image areas that are sharpest in this image", and lists one row per frame in the dock's order: a coloured chip, the file name, a bar and the count at the right edge. While a count is not final the row shows a spinner in place of the count. With no project open the overlay shows the centred panel "No stack loaded" over "Load a stack to count its sharp areas." and no rows.

The counts of the 8-frame folder are 0, 0, 4, 11, 0, 0, 0, 1 for 00.tif to 07.tif; the bar of a row is its count divided by the largest count (11), so 03.tif fills the track and the rows with 0 show an empty track. The 3-frame folder counts 0 for all three frames. No row shows a position in millimetres.

This path never starts a fuse: leave **Fuse stack** alone. The window's own close button in the title bar (`FFTabStrip::window-close`) is also labelled "Close"; this path never presses it, and every **Close** step below names `AppWindow::analytics-overlay` as its search scope.

## Open at launch and read the rows
1. Hover **Analytics** (`AppWindow::icon-bar`).
   Expect: the overlay titled "Analytics" covers the canvas with the heading "SHARP AREAS PER IMAGE" and the subtitle "Image areas that are sharpest in this image"; eight rows are listed at once in the order 00.tif to 07.tif, each with its chip and name, a bar and a spinner at the right edge but no number; no panel "No stack loaded"; the toolbar row, the right dock and the bottom bar are still drawn and the dock header reads "8 frames · 0 excluded"; the Analytics button is drawn selected (accent icon, accent bar at the icon bar's left edge).
2. Hover **Close** (`AppWindow::analytics-overlay`).
   Expect: (within 60 s) the spinners are gone and eight rows are listed in the order 00.tif, 01.tif, 02.tif, 03.tif, 04.tif, 05.tif, 06.tif, 07.tif, each row with a small coloured chip left of its name and its count at the right edge reading 0, 0, 4, 11, 0, 0, 0, 1.
3. Hover **Analytics** (`AppWindow::icon-bar`).
   Expect: the bar of 03.tif fills its track to the right end; the bar of 02.tif is shorter than that of 03.tif and longer than that of 07.tif; the rows 00.tif, 01.tif, 04.tif, 05.tif and 06.tif show an empty track; Must not: any row shows a figure in millimetres.
4. (human only) Look at the rows.
   Expect: the eight chips are eight different colours, the rows sit on an even pitch, the names start at one x, the bars start at one x and end at one x, and the counts are right-aligned.
## Close and reopen
5. Click **Close** (`AppWindow::analytics-overlay`).
   Expect: the overlay is gone, the canvas shows the frame again, and the Analytics button is drawn plain (no accent bar).
6. Click **Analytics** (`AppWindow::icon-bar`).
   Expect: the overlay titled "Analytics" is open with the button drawn selected and eight rows listed at once whose counts read 0, 0, 4, 11, 0, 0, 0, 1; Must not: a spinner is shown, or the panel "No stack loaded".
7. Click **Analytics** (`AppWindow::icon-bar`).
   Expect: the overlay is gone and the button is drawn plain.
8. Click **Analytics** (`AppWindow::icon-bar`).
   Expect: the overlay titled "Analytics" is open again with eight rows listed at once whose counts read 0, 0, 4, 11, 0, 0, 0, 1 and no spinner.
9. Press Escape.
   Expect: the overlay is gone and the button is drawn plain.
10. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: (within 60 s) the overlay titled "Analytics" lists eight rows whose counts read 0, 0, 4, 11, 0, 0, 0, 1 and no spinner is left.
## The count follows the selected tab
11. Click the first tab (`FFTab::area` 0).
    Expect: the dock header reads "3 frames · 0 excluded"; the overlay titled "Analytics" is still open with the button drawn selected and three rows listed, 00.tif, 01.tif and 02.tif, each with a spinner in place of its count; Must not: the eight rows of the other folder stay listed.
12. Hover **Close** (`AppWindow::analytics-overlay`).
    Expect: (within 60 s) three rows are listed, 00.tif, 01.tif and 02.tif, each with the count 0 and an empty track; Must not: the rows 03.tif to 07.tif are listed.
13. Hover the image row 01.tif in the right dock (`AppWindow::analytics-overlay` at window point 1175,380).
    Expect: the dock header still reads "3 frames · 0 excluded" and the rows 00.tif, 01.tif and 02.tif stay listed in the dock; the overlay titled "Analytics" is still open.
14. Click the second tab (`FFTab::area` 1).
    Expect: (within 60 s) the dock header reads "8 frames · 0 excluded" and eight rows are listed whose counts read 0, 0, 4, 11, 0, 0, 0, 1.
## One overlay at a time
15. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the projects overview (`AppWindow::projects-overlay`) shows the heading "Open projects"; the title "Analytics" is gone and the Analytics button is drawn plain.
16. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Analytics" is open and the heading "Open projects" is gone.
17. Click **Notifications** (`FFIconBar::bell`).
    Expect: the notification center opens beside the icon bar reading "Notifications"; the title "Analytics" is gone and the Analytics button is drawn plain.
18. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Analytics" is open and the notification center is gone.
19. Click **Step calculator** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Step calculator" is open, the title "Analytics" is gone, the Analytics button is drawn plain and the calculator button is drawn selected.
20. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Analytics" is open, the title "Step calculator" is gone, the calculator button is drawn plain and the Analytics button is drawn selected.
## Without a project
21. Click **Close** (`AppWindow::analytics-overlay`).
    Expect: the overlay is gone.
22. Click the "+" after the last tab (`FFTabStrip::plus-button`).
    Expect: the canvas and the bottom bar read "No project open".
23. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the Analytics button is enabled (`buttons AppWindow::icon-bar` lists **Analytics** without a leading `x`); the overlay titled "Analytics" opens with the heading "SHARP AREAS PER IMAGE" and the subtitle, and below them a centred panel with the bold line "No stack loaded" over the line "Load a stack to count its sharp areas.".
24. Hover **Close** (`AppWindow::analytics-overlay`).
    Expect: Must not: any row is listed, a spinner is shown or a count figure is shown; the panel "No stack loaded" stays.
25. Click the first tab (`FFTab::area` 0).
    Expect: the dock header reads "3 frames · 0 excluded"; the overlay titled "Analytics" is still open, the panel "No stack loaded" is gone and three rows are listed, 00.tif, 01.tif and 02.tif, each with the count 0 and no spinner, shown at once from the count kept in step 12.
26. Hover **Close** (`AppWindow::analytics-overlay`).
    Expect: the three rows 00.tif, 01.tif and 02.tif each still show the count 0 and no spinner is left.
27. Press Escape.
    Expect: the overlay is gone and the Analytics button is drawn plain.
