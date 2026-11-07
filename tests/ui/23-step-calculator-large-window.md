# Check the step calculator layout in a large window
Start: --open fixtures/mantis-64/frames --size 1920x1200

Use a fresh settings directory and a fresh data directory. This is the second window size of the step-calculator journey (21-step-calculator.md runs at the default window). The window opens at 1920 x 1200; the overlay (`AppWindow::calculator-overlay`) is then 1612 x 1080 and its drawing is at its largest scale. The calculator button is reached by its label **Step calculator** inside `AppWindow::icon-bar`. The five entered boxes are `FFField::input` 0 to 4 in layout order with the lens; with the objective the Magnification box is gone and the four boxes are `FFField::input` 0 Time per frame, 1 Numerical aperture (NA), 2 Overlap, 3 Depth range.

A typed value replaces the old one only after the old text is cleared: click the box, press Ctrl+a (lowercase a; the driver takes the key as written), press Backspace, then type. An empty box counts as "Not a number".

This path never starts a fuse: leave **Fuse stack** alone. The window's own close button in the title bar (`FFTabStrip::window-close`) is also labelled "Close"; every **Close** step below names `AppWindow::calculator-overlay` as its search scope.

## Open and read the Lens defaults
1. Click **Step calculator** (`AppWindow::icon-bar`).
   Expect: the overlay titled "Step calculator" covers the canvas; the right dock is still drawn.
2. Click **Lens (f-stop)** (`AppWindow::calculator-overlay`).
   Expect: the LENS (F-STOP) segment is highlighted; the CALCULATED row reads Numerical aperture 0.031, Depth of field 563.2 µm, Step 422.4 µm, Frames 13, Duration 13 s.
3. Click the Time per frame box (`FFField::input` 0).
   Expect: the five boxes read 1, 1, 8, 0.25 and 5 in the order Time per frame, Magnification, f-stop, Overlap, Depth range; no line is drawn under any box.
4. (human only) Look at the overlay.
   Expect: the illustration, its leader lines, the inset "FIRST TWO PLANES, ENLARGED" and the dimension marks are clearly larger than in the default window and sit centred in the overlay, with the same margin to the left and the right; every entered box is as tall as the Optics switch and has the size it has in the default window; each box sits at the end of its leader line; lines and text are sharp, nothing overlaps the SUMMARY band or the Optics switch.
## Objective optics and back
5. Click **Objective (NA)** (`AppWindow::calculator-overlay`).
   Expect: the OBJECTIVE (NA) segment is highlighted; the label "Magnification (x)" is gone; the label "Numerical aperture (NA)" shows with a box reading 0.25; the CALCULATED row reads Depth of field 8.8 µm, Step 6.6 µm, Frames 759, Duration 759 s.
6. (human only) Look at the overlay.
   Expect: the drawing is still centred and as large as with the lens; the objective illustration has no leader line to a Magnification box; each of the four boxes sits at the end of its leader line.
7. Click **Lens (f-stop)** (`AppWindow::calculator-overlay`).
   Expect: the LENS (F-STOP) segment is highlighted; the label "Magnification (x)" and its box are back.
## Overlap violation
8. Click the Overlap box (`FFField::input` 3).
   Expect: the box reads 0.25.
9. Press Ctrl+a (lowercase a).
   Expect: the text in the Overlap box is drawn selected.
10. Press Backspace.
    Expect: Must not: new calculated values; the Overlap box is empty with a red border, the line "Not a number" shows under it, and the CALCULATED row still reads Step 422.4 µm and Frames 13.
11. Type "0,95" into the Overlap box (`FFField::input` 3).
    Expect: Must not: 0.95 is planned; the box reads 0.95 (point, not comma) with a red border and the line "value 0.95 is outside the allowed range [0, 0.9]" shows under it inside the inset card, the ENTERED row reads Overlap 0.9 (the last valid keystroke), and the CALCULATED row reads Step 56.32 µm, Frames 90 and Duration 90 s.
12. (human only) Look at the overlay.
    Expect: the inset card is taller to hold the line, the dotted line from the card to the bracket below it keeps its length, and the Frames and Depth range captions stay clear of the SUMMARY band.
13. Press Ctrl+a (lowercase a).
    Expect: the text in the Overlap box is drawn selected.
14. Press Backspace.
    Expect: the box is empty and the line under it reads "Not a number".
15. Type "0.25" into the Overlap box (`FFField::input` 3).
    Expect: the line under the box is gone and the box has no red border; the CALCULATED row reads Step 422.4 µm and Frames 13.
## Close
16. Click **Close** (`AppWindow::calculator-overlay`).
    Expect: the overlay is gone and the calculator button is drawn plain.
