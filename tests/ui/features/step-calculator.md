# Step calculator
Reach: press the calculator button at the top of the icon bar on the left edge (label **Step calculator**, inside `AppWindow::icon-bar`); it shows with or without a project. The same button, **Close** (top right of the overlay) and Escape close it.
Sub-features: the Optics switch LENS (F-STOP) | OBJECTIVE (NA); five entered boxes (Time per frame, Magnification, f-stop or Numerical aperture, Overlap, Depth range) that re-plan on every keystroke; the SUMMARY band (ENTERED and CALCULATED rows: Numerical aperture only with the lens, Depth of field, Step, Frames, Duration); the violation line under a box holding an out-of-range or unparsable value; a comma accepted as the decimal point; one full-canvas overlay at a time (the projects overview and the notification center close it, and it closes them).
Ids: `AppWindow::icon-bar` (the button is clicked by its label), `AppWindow::calculator-overlay` (**Close**, **Lens (f-stop)** and **Objective (NA)** are clicked by label inside it), the five boxes `FFField::input` 0 to 4 in layout order (Magnification is 1 and keeps its place while hidden by the objective).
Gotchas:
- The overlay covers the canvas only: the toolbar, the dock and the bottom bar stay visible. There is no **Apply values** button and no Settle time box.
- A box keeps the typed text and replaces the old value only after the old text is cleared: click the box, `Press` Ctrl+A, `Press` Backspace, then `Type`. An empty box shows "Not a number".
- Calculated values keep the last valid plan while a box holds a violation; the ENTERED row shows the last valid value too.
- The illustration (planes, leader lines, inset, dimension marks) is judged by eye and is `(human only)`.
- The drawing is centred and grows with the window (1.0 at 1200 x 760 up to 1.3), the entered boxes keep their size. The default window is one size; the larger window is a second guide run started with `--size` (23-step-calculator-large-window.md), since the driver cannot resize a window.
- Opening the overlay with a driver needs a build with the `mcp` feature (`tools/gui/run-gui.sh` builds it); a binary built without it answers no request.
Guides: 21-step-calculator.md, 23-step-calculator-large-window.md
Book: guide/gui.md#plan-a-stack-with-the-step-calculator
