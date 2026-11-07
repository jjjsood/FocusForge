# Plan a focus stack with the step calculator
Start: --open fixtures/mantis-64/frames

Use a fresh settings directory and a fresh data directory. The calculator button sits at the top of the 48 px icon bar on the left edge, above the bell and Settings; it is reached by its label **Step calculator** inside `AppWindow::icon-bar`. The overlay (`AppWindow::calculator-overlay`) covers the canvas below the toolbar and above the bottom bar; the right dock stays visible. While it is open the button is drawn selected: an accent-coloured icon and a thin accent bar on the icon bar's left edge.

The overlay holds an Optics switch (LENS (F-STOP) | OBJECTIVE (NA)), five entered boxes, a SUMMARY band with an ENTERED row and a CALCULATED row, and the **Close** button (label "Close", top right). With the lens the five boxes are `FFField::input` 0 to 4 in layout order: Time per frame (s), Magnification (x), f-stop, Overlap (fraction of the depth of field), Depth range (mm). With the objective the Magnification box and its label are gone and do not count: the four boxes are `FFField::input` 0 Time per frame, 1 Numerical aperture (NA), 2 Overlap, 3 Depth range. Defaults: 1, 1, 8, 0.25, 5 (objective NA 0.25). A box shows what was typed; a comma counts as the decimal point and the box then shows a point. Every keystroke re-plans: a value outside the allowed range or text that is not a number draws the box red and a line under it, and the calculated values keep the last valid plan, which can be an earlier keystroke of the same entry.

This path never starts a fuse: leave **Fuse stack** alone. The window's own close button in the title bar (`FFTabStrip::window-close`) is also labelled "Close"; this path never presses it, and every **Close** step below names `AppWindow::calculator-overlay` as its search scope.

A typed value replaces the old one only after the old text is cleared: click the box, press Ctrl+a (lowercase a; the driver takes the key as written), press Backspace, then type. An empty box counts as "Not a number".

## Open and read the Lens defaults
1. Click **Step calculator** (`AppWindow::icon-bar`).
   Expect: the overlay titled "Step calculator" covers the canvas; the toolbar row above it and the right dock are still drawn and the dock header still reads "3 frames · 0 excluded"; the calculator button is drawn selected (accent icon, accent bar at the icon bar's left edge).
2. Click **Lens (f-stop)** (`AppWindow::calculator-overlay`).
   Expect: the LENS (F-STOP) segment is highlighted; the CALCULATED row reads Numerical aperture 0.031, Depth of field 563.2 µm, Step 422.4 µm, Frames 13, Duration 13 s.
3. Click the Time per frame box (`FFField::input` 0).
   Expect: the five boxes read 1, 1, 8, 0.25 and 5 in the order Time per frame, Magnification, f-stop, Overlap, Depth range; the ENTERED row reads Optics f-stop, Magnification 1×, f-stop 8, Overlap 0.25, Depth range 5 mm, Time per frame 1 s; no line is drawn under any box.
4. (human only) Look at the illustration.
   Expect: the lens illustration (camera on a rail, a subject) shows 13 evenly spaced focus planes, leader lines from the boxes to their parts, an inset titled "FIRST TWO PLANES, ENLARGED", dimension marks reading Step 422.4 µm, Depth of field 563.2 µm and "Frames 13 Duration 13 s", and a legend "Entered" and "Calculated". The drawing sits centred in the overlay body, and every entered box is as tall as the Optics switch (28 px).
## Must not work: no Apply values
5. Hover **Close** (`AppWindow::calculator-overlay`), then look for an **Apply values** button.
   Expect: Must not: an **Apply values** button or the caption "Fills Step and sets End = Start + depth range" exists; `buttons AppWindow::calculator-overlay` lists only **Close**, **Lens (f-stop)** with checked=true and **Objective (NA)** with checked=false.
## Close and reopen
6. Click **Close** (`AppWindow::calculator-overlay`).
   Expect: the overlay is gone, the canvas shows the frame again, and the calculator button is drawn plain (no accent bar).
7. Click **Step calculator** (`AppWindow::icon-bar`).
   Expect: the overlay titled "Step calculator" is open again with the button drawn selected; the CALCULATED row reads Numerical aperture 0.031 and Frames 13.
8. Click **Step calculator** (`AppWindow::icon-bar`).
   Expect: the overlay is gone and the button is drawn plain.
9. Click **Step calculator** (`AppWindow::icon-bar`).
   Expect: the overlay is open.
10. Click the Time per frame box (`FFField::input` 0).
    Expect: the overlay is still open and the box still reads 1.
11. Press Escape.
    Expect: the overlay is gone and the button is drawn plain.
12. Click **Step calculator** (`AppWindow::icon-bar`).
    Expect: the overlay is open with LENS (F-STOP) highlighted.
## Objective optics
13. Click **Objective (NA)** (`AppWindow::calculator-overlay`).
    Expect: the OBJECTIVE (NA) segment is highlighted; the label "Magnification (x)" is gone; the label "Numerical aperture (NA)" shows with a box reading 0.25; the ENTERED row reads Optics NA; the CALCULATED row has no Numerical aperture cell and reads Depth of field 8.8 µm, Step 6.6 µm, Frames 759, Duration 759 s.
14. Click the Depth range box (`FFField::input` 3).
    Expect: the box reads 5 and the CALCULATED row is unchanged.
15. Press Ctrl+a (lowercase a).
    Expect: the text in the Depth range box is drawn selected.
16. Press Backspace.
    Expect: Must not: new calculated values; the Depth range box is empty with a red border, the line "Not a number" shows under it, and the CALCULATED row still reads Depth of field 8.8 µm, Step 6.6 µm, Frames 759, Duration 759 s.
17. Type "0.1" into the Depth range box (`FFField::input` 3).
    Expect: the line under the box is gone; the box reads 0.1; the ENTERED row reads Depth range 0.1 mm; the CALCULATED row reads Depth of field 8.8 µm, Step 6.6 µm, Frames 17, Duration 17 s.
18. (human only) Look at the illustration.
    Expect: the objective illustration (an objective in place of the lens) shows 17 evenly spaced focus planes and has no leader line to a Magnification box.
## Out-of-range entry and its correction
19. Click **Lens (f-stop)** (`AppWindow::calculator-overlay`).
    Expect: the LENS (F-STOP) segment is highlighted; the label "Magnification (x)" and its box are back; the CALCULATED row reads Numerical aperture 0.031, Depth of field 563.2 µm, Step 422.4 µm, Frames 2, Duration 2 s (the range 0.1 mm was kept).
20. Click the Overlap box (`FFField::input` 3).
    Expect: the box reads 0.25.
21. Press Ctrl+a (lowercase a).
    Expect: the text in the Overlap box is drawn selected.
22. Press Backspace.
    Expect: Must not: new calculated values; the Overlap box is empty with a red border, the line "Not a number" shows under it, and the CALCULATED row still reads Step 422.4 µm and Frames 2.
23. Type "0,95" into the Overlap box (`FFField::input` 3).
    Expect: Must not: 0.95 is planned; the box reads 0.95 (point, not comma) with a red border, the line "value 0.95 is outside the allowed range [0, 0.9]" shows under it inside the inset card, the ENTERED row reads Overlap 0.9 (the last valid keystroke), and the CALCULATED row reads Numerical aperture 0.031, Depth of field 563.2 µm, Step 56.32 µm, Frames 3, Duration 3 s.
24. Click the Overlap box (`FFField::input` 3).
    Expect: the box still reads 0.95 and the line is still shown.
25. Press Ctrl+a (lowercase a).
    Expect: the text in the Overlap box is drawn selected.
26. Press Backspace.
    Expect: the box is empty and the line under it reads "Not a number".
27. Type "0,3" into the Overlap box (`FFField::input` 3).
    Expect: the line under the box is gone and the box has no red border; the box reads 0.3; the ENTERED row reads Overlap 0.3; the CALCULATED row reads Depth of field 563.2 µm, Step 394.24 µm, Frames 2.
## One overlay at a time
28. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the projects overview (`AppWindow::projects-overlay`) shows the heading "Open projects"; the title "Step calculator" is gone and the calculator button is drawn plain.
29. Click **Step calculator** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Step calculator" is open and the heading "Open projects" is gone.
30. Click **Notifications** (`FFIconBar::bell`).
    Expect: the notification center opens beside the icon bar reading "Notifications"; the title "Step calculator" is gone and the calculator button is drawn plain.
31. Click **Step calculator** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Step calculator" is open and the notification center is gone.
## Without a project
32. Click **Close** (`AppWindow::calculator-overlay`).
    Expect: the overlay is gone.
33. Click the "+" after the last tab (`FFTabStrip::plus-button`).
    Expect: the canvas and the bottom bar read "No project open".
34. Click **Step calculator** (`AppWindow::icon-bar`).
    Expect: the overlay titled "Step calculator" is open over the empty window and the calculator button is drawn selected; the Optics switch shows LENS (F-STOP) and OBJECTIVE (NA).
35. Press Escape.
    Expect: the overlay is gone and the calculator button is drawn plain.
