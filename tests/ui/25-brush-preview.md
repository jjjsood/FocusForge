# Preview the brush source inside the brush ring
Start: --open fixtures/mantis-512/frames --open fixtures/mantis-64/frames --fuse-tabs 0,1 --enter-editor --size 1200x700
The preview is the picture drawn inside the brush ring of the Composite while the Preview switch is on and the pointer rests on an image point. "The preview is present" means `shot <png> --crop FFBrushCursor::preview` writes a picture; "the preview is absent" means that call reports `0 elements with id 'FFBrushCursor::preview'`. A click on the Preview switch or on **Pipette** changes the preview at the next pointer move over a pane (or at **Press p**), so a check follows a hover. The panes of the 1200x700 window: the frame pane starts at window point 64,92 and the Composite at 502,92, each 422 x 608; the image fills the pane's full width, so a point near a pane corner lies outside the image.
## Enter the editor on both tabs
1. Click **Keep** (`AppWindow::divergence-warning`) once the dialog "Keep this fused result?" shows (both fuses run after launch; the tab with 3 frames is the active one).
   Expect: (within 120 s) the dialog is gone; the dock list shows the heading "3 frames"; the CHECKPOINTS list shows 1 row.
2. Click the first tab (`FFTab::area` 0).
   Expect: (within 180 s) the dialog "Keep this fused result?" is shown and the dock header reads "8 frames · 0 excluded".
3. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone; the dock list shows the heading "8 frames"; the CHECKPOINTS list shows 1 row; `buttons AppWindow::editor-toolbar` lists **Preview** with checked=false and **Undo**, **Redo** and **Save checkpoint** with enabled false.
## The switch and the p key
4. Hover the Composite at its centre, window point 713,396 (`FFCanvas::pane-right`), with the Preview switch off.
   Expect: Must not: a preview shows; the preview is absent.
5. Click **Preview** (`FFEditorToolbar::preview-switch`).
   Expect: `buttons AppWindow::editor-toolbar` lists **Preview** with checked=true.
6. Press p.
   Expect: `buttons AppWindow::editor-toolbar` lists **Preview** with checked=false.
7. Press p.
   Expect: `buttons AppWindow::editor-toolbar` lists **Preview** with checked=true.
## The switch on the other tab
8. Click the second tab (`FFTab::area` 1).
   Expect: the dock list shows the heading "3 frames"; `buttons AppWindow::editor-toolbar` lists **Preview** with checked=true.
9. Click the first tab (`FFTab::area` 0).
   Expect: the dock list shows the heading "8 frames"; `buttons AppWindow::editor-toolbar` lists **Preview** with checked=true.
## The preview follows the pointer
10. Hover the Composite at its centre, window point 713,396 (`FFCanvas::pane-right`).
    Expect: the preview is present; a full-window shot shows the brush ring at the centre of both panes.
11. Hover the Composite at window point 608,290, up and left of its centre (`FFCanvas::pane-right`).
    Expect: the preview is present; a full-window shot shows the brush ring up and left of the centre in both panes.
12. Hover the frame pane at its centre, window point 275,396 (`FFCanvas::pane-left`).
    Expect: the preview is present (the pointer over the frame pane carries it to the Composite's ring); a full-window shot shows the brush ring at the centre of both panes.
13. (human only) Hold Ctrl and roll the mouse wheel down one notch over the Composite, then up one notch.
    Expect: the left pane label reads "01.tif" after the first notch and "00.tif" after the second; the ring's inside shows the new frame as soon as the left pane does.
14. (human only) Move the pointer slowly across the Composite, then across the frame pane.
    Expect: inside the Composite's ring the selected frame shows at about 80 % over the result and fades at the ring's soft edge, following the pointer; the frame pane's ring shows no second picture.
## Where the preview is hidden
15. Hover the Composite at window point 506,96, 4 px inside its top-left corner, outside the image (`FFCanvas::pane-right`).
    Expect: Must not: a preview shows; the preview is absent.
16. Click **Pipette** (`FFEditorToolbar::pipette`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Pipette** with checked=true and **Preview** with checked=true.
17. Hover the Composite at its centre, window point 713,396 (`FFCanvas::pane-right`), with the pipette armed.
    Expect: Must not: a preview shows; the preview is absent, **Pipette** still lists checked=true and the left pane label still reads "00.tif".
18. Click **Pipette** (`FFEditorToolbar::pipette`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Pipette** with checked=false.
19. Hover the Composite at its centre, window point 713,396 (`FFCanvas::pane-right`).
    Expect: the preview is present.
20. (human only) Hold Alt with the pointer resting on the Composite, then release it.
    Expect: while Alt is held the pipette cursor shows and the ring has no picture; after the release the ring with its picture is back.
## The result in its own window
21. Click the Composite's undock control (`FFUndockControl::area` 1).
    Expect: (within 5 s) a second window titled "FocusForge — Composite" opens; the main window shows one pane, labelled "00.tif".
22. Hover the second window's pane (`ComparisonWindow::input`, in the second window, `--window 1`).
    Expect: the preview is present in the second window (`shot <png> --window 1 --crop FFBrushCursor::preview` writes a picture) and absent in the main window (`shot <png> --window 0 --crop FFBrushCursor::preview` reports `0 elements`).
23. Hover the main window's pane (`FFCanvas::pane-left`, `--window 0`).
    Expect: Must not: the main window shows a preview (its crop shot reports `0 elements`); the second window's crop shot still writes a picture.
24. Click "Dock pane" (`FFUndockControl::area` 0, in the second window, `--window 1`).
    Expect: the main window shows two panes, the frame pane and the pane labelled "Composite"; `buttons AppWindow::editor-toolbar` lists **Undo**, **Redo** and **Save checkpoint** with enabled false; the CHECKPOINTS list shows 1 row.
## Hovering changed nothing
25. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
    Expect: the dialog "Export image" shows the line "Exporting the unsaved fuse result held in this tab."; the line "Exporting your current unsaved retouch edits." is not shown.
26. Press Escape.
    Expect: Must not: an edit shows; the dialog is gone, `buttons AppWindow::editor-toolbar` lists **Undo**, **Redo** and **Save checkpoint** with enabled false and the CHECKPOINTS list shows 1 row.
## A stroke with the preview on
27. Drag across the Composite from its centre to window point 880,400 (`FFCanvas::pane-right`).
    Expect: (within 120 s) `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled true and **Redo** with enabled false.
28. Click **Undo** (`FFEditorToolbar::undo`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Undo** with enabled false and **Redo** with enabled true.
29. Click **Redo** (`FFEditorToolbar::redo`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Redo** with enabled false and **Undo** with enabled true.
30. Hover the Composite at its centre, window point 713,396 (`FFCanvas::pane-right`).
    Expect: the preview is present after the stroke, the undo and the redo; **Preview** still lists checked=true.
31. (human only) Press in the Composite, move the pointer slowly, hold still, then release.
    Expect: the ring has no picture while the button is down; the picture is back after the release.
## The switch off
32. Press p.
    Expect: `buttons AppWindow::editor-toolbar` lists **Preview** with checked=false; the preview is absent although the pointer still rests on the Composite.
33. Hover the Composite at its centre, window point 713,396 (`FFCanvas::pane-right`), with the switch off.
    Expect: Must not: a preview shows; the preview is absent.
34. Press p.
    Expect: `buttons AppWindow::editor-toolbar` lists **Preview** with checked=true.
35. (human only) Quit without saving (**Close** `FFTabStrip::window-close`, then **Quit without saving**), start the same command again with the same settings folder and click **Keep** on both tabs.
    Expect: the Preview switch's knob sits at its right end (on).
