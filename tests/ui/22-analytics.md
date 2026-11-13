# Count the sharp areas of every frame in the Analytics overlay
Start: --open fixtures/mantis-64/frames --open fixtures/mantis-512/frames --analytics --analytics-hold-ms 25000

Use a fresh settings directory and a fresh data directory. Two folders are open: the first tab holds 3 frames, the second (selected) holds 8 frames. The overlay starts open once both folders are loaded. Every count is held back for 25 seconds, so the rows stay pending (a sweeping bar and a spinner in place of the count, no number) and the overlap bar stays pending (neutral segments, no figure) until a count has finished once. The count follows the selected tab: it starts when a stack is loaded, a count that was left unfinished by opening another folder starts again when its tab is selected, and a finished count is kept and shown at once on every later opening. Steps that wait for the rows say `(within 60 s)`.

The Analytics button sits in the 48 px icon bar on the left edge, directly under the step calculator button, above the bell and Settings; it is reached by its label **Analytics** inside `AppWindow::icon-bar`, and it is never greyed out. The overlay (`AppWindow::analytics-overlay`) covers the canvas below the toolbar and above the bottom bar; the right dock stays visible. While it is open the button is drawn selected: an accent-coloured icon and a thin accent bar on the icon bar's left edge. The overlay is titled "Analytics". With a stack of at least two images it shows the overlap bar block above the list, both while counting and once the counts are final: the heading "SHARPNESS OVERLAP", the caption "Share of sharp areas that neighbouring images have in common", the legend chips "0 %", "5 %" and "20 %+", and the bar with one segment per neighbour pair and the image numbers 1 to N on the segment boundaries. Below it come the heading "SHARP AREAS PER IMAGE" with the subtitle "Image areas that are sharpest in this image" and one row per frame in the dock's order: a coloured chip, the file name, a bar and the count at the right edge. While a count is not final the row shows a spinner in place of the count and the bar segments are neutral with no figure. With no project open the overlay shows only the centred panel "No stack loaded" over "Load a stack to analyse it.": no heading, no caption, no bar and no rows.

The segments and their texts are not buttons: every bar step reads a screenshot. A finished segment with data shows its whole-percent share; a segment of a pair without any sharp area shows a hollow outline with a dash "–". Hovering a finished segment (`FFOverlapBar::area`, whose centre is the middle pair on 8 images, or a window point to pick another pair) shows a two-line tooltip; a pending bar shows none.

The counts of the 8-frame folder are 0, 0, 4, 11, 0, 0, 0, 1 for 00.tif to 07.tif; the bar of a row is its count divided by the largest count (11), so 03.tif fills the track and the rows with 0 show an empty track. The overlap bar of the 8-frame folder reads, left to right, "0 %", "0 %", "0 %", "0 %", "–", "0 %", "100 %". The 3-frame folder counts 0 for all three frames and its two bar segments both read "–". No row shows a position in millimetres.

This path never starts a fuse: leave **Fuse stack** alone. The window's own close button in the title bar (`FFTabStrip::window-close`) is also labelled "Close"; this path never presses it, and every **Close** step below names `AppWindow::analytics-overlay` as its search scope.

## Open at launch and read the rows
1. Hover **Analytics** (`AppWindow::icon-bar`).
   Expect: the overlay titled "Analytics" covers the canvas with the heading "SHARPNESS OVERLAP" and, below the bar block, the heading "SHARP AREAS PER IMAGE" with the subtitle "Image areas that are sharpest in this image"; the bar shows seven unfilled segments with a soft moving band and the numbers 1 to 8 beneath, and no figure in any segment; Must not: a percent figure or a dash is shown in the bar; eight rows are listed at once in the order 00.tif to 07.tif, each with its chip and name, a bar and a spinner at the right edge but no number; no panel "No stack loaded"; the toolbar row, the right dock and the bottom bar are still drawn and the dock header reads "8 frames · 0 excluded"; the Analytics button is drawn selected (accent icon, accent bar at the icon bar's left edge).
2. Hover **Close** (`AppWindow::analytics-overlay`).
   Expect: (within 60 s) the spinners are gone and eight rows are listed in the order 00.tif, 01.tif, 02.tif, 03.tif, 04.tif, 05.tif, 06.tif, 07.tif, each row with a small coloured chip left of its name and its count at the right edge reading 0, 0, 4, 11, 0, 0, 0, 1.
3. Hover **Analytics** (`AppWindow::icon-bar`).
   Expect: the bar of 03.tif fills its track to the right end; the bar of 02.tif is shorter than that of 03.tif and longer than that of 07.tif; the rows 00.tif, 01.tif, 04.tif, 05.tif and 06.tif show an empty track; Must not: any row shows a figure in millimetres.
4. (human only) Look at the rows.
   Expect: the eight chips are eight different colours, the rows sit on an even pitch, the names start at one x, the bars start at one x and end at one x, and the counts are right-aligned.
## The overlap bar
5. Hover **Close** (`AppWindow::analytics-overlay`).
   Expect: (within 60 s) above the heading "SHARP AREAS PER IMAGE" sit the heading "SHARPNESS OVERLAP", the caption "Share of sharp areas that neighbouring images have in common" and the legend chips "0 %", "5 %" and "20 %+"; the bar has seven segments reading, left to right, "0 %", "0 %", "0 %", "0 %", "–", "0 %", "100 %", with the numbers 1, 2, 3, 4, 5, 6, 7, 8 beneath the boundaries; Must not: the bar shows a moving band or fewer than seven segments.
6. Hover the overlap bar (`FFOverlapBar::area`).
   Expect: (within 5 s) a tooltip with two lines appears, "Images 4 and 5 · 03.tif, 04.tif" over "0 % – likely a gap in focus here".
7. Hover the dash segment between the numbers 5 and 6 of the overlap bar (`FFOverlapBar::area` at window point 617,185).
   Expect: (within 5 s) a tooltip with two lines appears, "Images 5 and 6 · 04.tif, 05.tif" over "No sharp areas in either image"; Must not: the second line names a percentage.
8. (human only) Look at the bar and its legend.
   Expect: the legend chips run from red ("0 %") over yellow ("5 %") to green ("20 %+"); the segments of the pairs 3-4 and 4-5 are full red, those of 1-2, 2-3 and 6-7 a pale red, the segment of 5-6 an outline only, and the segment of 7-8 a pale green.
## Close and reopen
9. Click **Close** (`AppWindow::analytics-overlay`).
   Expect: the overlay is gone, the canvas shows the frame again, and the Analytics button is drawn plain (no accent bar).
10. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Analytics" is open with the button drawn selected and eight rows listed at once whose counts read 0, 0, 4, 11, 0, 0, 0, 1; Must not: a spinner is shown, or the panel "No stack loaded".
11. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay is gone and the button is drawn plain.
12. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Analytics" is open again with eight rows listed at once whose counts read 0, 0, 4, 11, 0, 0, 0, 1 and no spinner.
13. Press Escape.
    Expect: the overlay is gone and the button is drawn plain.
14. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: (within 60 s) the overlay titled "Analytics" lists eight rows whose counts read 0, 0, 4, 11, 0, 0, 0, 1 and no spinner is left.
## The count follows the selected tab
15. Click the first tab (`FFTab::area` 0).
    Expect: the dock header reads "3 frames · 0 excluded"; the overlay titled "Analytics" is still open with the button drawn selected, the bar shows two unfilled segments with the numbers 1 to 3 beneath and no figure, and three rows are listed, 00.tif, 01.tif and 02.tif, each with a spinner in place of its count; Must not: the eight rows of the other folder stay listed, or a percent figure is shown in the bar.
16. Hover **Close** (`AppWindow::analytics-overlay`).
    Expect: (within 60 s) the bar shows two hollow segments, each with a dash "–" and no percent, and the numbers 1 to 3 beneath; three rows are listed, 00.tif, 01.tif and 02.tif, each with the count 0 and an empty track; Must not: the rows 03.tif to 07.tif are listed.
17. Hover the image row 01.tif in the right dock (`AppWindow::analytics-overlay` at window point 1175,380).
    Expect: the dock header still reads "3 frames · 0 excluded" and the rows 00.tif, 01.tif and 02.tif stay listed in the dock; the overlay titled "Analytics" is still open.
18. Click the second tab (`FFTab::area` 1).
    Expect: (within 60 s) the dock header reads "8 frames · 0 excluded", the bar reads "0 %", "0 %", "0 %", "0 %", "–", "0 %", "100 %" with the numbers 1 to 8 beneath, and eight rows are listed whose counts read 0, 0, 4, 11, 0, 0, 0, 1.
## One overlay at a time
19. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the projects overview (`AppWindow::projects-overlay`) shows the heading "Open projects"; the title "Analytics" is gone and the Analytics button is drawn plain.
20. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Analytics" is open and the heading "Open projects" is gone.
21. Click **Notifications** (`FFIconBar::bell`).
    Expect: the notification center opens beside the icon bar reading "Notifications"; the title "Analytics" is gone and the Analytics button is drawn plain.
22. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Analytics" is open and the notification center is gone.
23. Click **Step calculator** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Step calculator" is open, the title "Analytics" is gone, the Analytics button is drawn plain and the calculator button is drawn selected.
24. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Analytics" is open, the title "Step calculator" is gone, the calculator button is drawn plain and the Analytics button is drawn selected.
## Without a project
25. Click **Close** (`AppWindow::analytics-overlay`).
    Expect: the overlay is gone.
26. Click the "+" after the last tab (`FFTabStrip::plus-button`).
    Expect: the canvas and the bottom bar read "No project open".
27. Click **Analytics** (`AppWindow::icon-bar`).
    Expect: the Analytics button is enabled (`buttons AppWindow::icon-bar` lists **Analytics** without a leading `x`); the overlay titled "Analytics" opens with only a centred panel holding the bold line "No stack loaded" over the line "Load a stack to analyse it.".
28. Hover **Close** (`AppWindow::analytics-overlay`).
    Expect: Must not: the heading "SHARPNESS OVERLAP", the heading "SHARP AREAS PER IMAGE", an overlap bar, any row, a spinner or a count figure is shown; the panel "No stack loaded" stays.
29. Click the first tab (`FFTab::area` 0).
    Expect: the dock header reads "3 frames · 0 excluded"; the overlay titled "Analytics" is still open, the panel "No stack loaded" is gone, (within 3 s) the bar shows two hollow segments each with a dash "–" and the numbers 1 to 3 beneath, and three rows are listed, 00.tif, 01.tif and 02.tif, each with the count 0 and no spinner, shown at once from the count kept in step 16.
30. Hover **Close** (`AppWindow::analytics-overlay`).
    Expect: the three rows 00.tif, 01.tif and 02.tif each still show the count 0 and no spinner is left.
31. Press Escape.
    Expect: the overlay is gone and the Analytics button is drawn plain.
