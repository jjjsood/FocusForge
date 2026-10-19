# Retouch a fused result in the editor
Start: --open fixtures/mantis-64/frames --fuse-tabs 0 --enter-editor
## Enter the editor
1. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog "Keep this fused result?" is gone; the bottom bar reads "fused" followed by a time (for example "fused 16:34"); the toolbar row shows the captions "Size" and "Soft"; the CHECKPOINTS list in the right dock shows 1 row (within 60 s).
2. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the toolbar row shows the **Fuse stack** button at its left and no longer shows "Size" or "Soft"; **Save checkpoint** at its right end is plain greyed text, not a button.
3. Click the Composite pane (`FFCanvas::pane-right`) while STACKING is selected.
   Expect: Must not: the toolbar row shows "Size", "Soft" or **Undo**; it still shows the **Fuse stack** button.
4. Click **EDITOR** (`FFModeSwitch::editor-half`).
   Expect: the toolbar row shows "Size" with "60", "Soft" with "30%" and **Save checkpoint**, and no longer shows **Fuse stack**; `buttons AppWindow::editor-toolbar` lists **Undo**, **Redo** and **Save checkpoint** with enabled false (nothing was painted); the CHECKPOINTS list shows 1 row.
5. Click **Undo** (`FFEditorToolbar::undo`).
   Expect: Must not: **Redo** becomes enabled; `buttons AppWindow::editor-toolbar` still lists **Undo** and **Redo** with enabled false.
6. Click **Save checkpoint** (`AppWindow::editor-toolbar`) before any stroke.
   Expect: Must not: the CHECKPOINTS list grows; it still shows 1 row and `buttons AppWindow::editor-toolbar` lists **Save checkpoint** with enabled false.
## Brush size and softness
7. Drag the Size slider thumb a quarter of the way along its track, to window point 122,56 (`FFSlider::drag-area` 0).
   Expect: the text beside the Size slider reads "3"; the text beside the Soft slider still reads "30%".
8. Drag the Size slider thumb to the left end of its track, to window point 104,56 (`FFSlider::drag-area` 0).
   Expect: the text beside the Size slider reads "1".
9. Drag the Size slider thumb to the right end of its track, to window point 176,56 (`FFSlider::drag-area` 0).
   Expect: the text beside the Size slider reads "60".
10. Drag the Soft slider thumb a quarter of the way along its track, to window point 302,56 (`FFSlider::drag-area` 1).
    Expect: the text beside the Soft slider reads "25%"; the text beside the Size slider still reads "60".
11. Drag the Soft slider thumb to the right end of its track, to window point 356,56 (`FFSlider::drag-area` 1).
    Expect: the text beside the Soft slider reads "100%".
12. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the toolbar row shows the **Fuse stack** button and no longer shows "Size" or "Soft"; the dock shows the tabs **Images** and **Settings**.
13. Click **Settings** (`FFDockTabs::settings-tab`).
    Expect: the dock shows a field labelled "Algorithm" under the heading FUSE.
14. Click **EDITOR** (`FFModeSwitch::editor-half`).
    Expect: the toolbar row shows "Size" with "60" and "Soft" with "100%" (the values are kept); `buttons AppWindow::editor-toolbar` lists **Undo**, **Redo** and **Save checkpoint** with enabled false; the dock shows no **Images** or **Settings** tab.
## Source frame
15. Click the third frame row (`FFImageRow::area` 2).
    Expect: the dock list shows 3 rows; the left pane label (`FFCanvas::pane-left`) reads "02.tif".
16. Click the second frame row (`FFImageRow::area` 1).
    Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif".
17. Click the first frame row (`FFImageRow::area` 0).
    Expect: the left pane label (`FFCanvas::pane-left`) reads "00.tif".
18. Click **Pipette** (`FFEditorToolbar::pipette`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Pipette** with checked=true.
19. Click **Pipette** again (`FFEditorToolbar::pipette`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Pipette** with checked=false.
20. Alt+click the Composite pane (`FFCanvas::pane-right`).
    Expect: the left pane label shows one of "00.tif", "01.tif" or "02.tif", the frame picked at that point; `buttons AppWindow::editor-toolbar` still lists **Pipette** with checked=false.
## Stroke, undo and redo
21. Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
    Expect: (within 120 s) `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled true and **Redo** with enabled false; **Undo** is drawn bright on a filled button.
22. Click **Undo** (`FFEditorToolbar::undo`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Undo** with enabled false and **Redo** with enabled true.
23. Click **Undo** again while it is greyed out (`FFEditorToolbar::undo`).
    Expect: Must not: **Undo** becomes enabled or **Redo** becomes greyed out; `buttons AppWindow::editor-toolbar` still lists **Undo** with enabled false and **Redo** with enabled true.
24. Click **Redo** (`FFEditorToolbar::redo`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Redo** with enabled false and **Undo** with enabled true.
25. Click **Redo** again while it is greyed out (`FFEditorToolbar::redo`).
    Expect: Must not: **Redo** becomes enabled or **Undo** becomes greyed out; `buttons AppWindow::editor-toolbar` still lists **Redo** with enabled false and **Undo** with enabled true.
26. Click **Undo** (`FFEditorToolbar::undo`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Undo** with enabled false and **Redo** with enabled true.
27. Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
    Expect: Must not: **Redo** stays enabled; (within 120 s) `buttons AppWindow::editor-toolbar` lists **Redo** with enabled false and **Undo** with enabled true.
28. (human only) Drag a long curve across the Composite pane, holding the button down for several seconds.
    Expect: a ring outline follows the pointer for the whole drag and the Composite shows a soft trail along the curve.
## Save a checkpoint
29. Click **Save checkpoint** (`AppWindow::editor-toolbar`).
    Expect: the CHECKPOINTS list shows 2 rows; `buttons AppWindow::editor-toolbar` lists **Save checkpoint**, **Undo** and **Redo** with enabled false.
30. Click **Save checkpoint** (`AppWindow::editor-toolbar`) again without a new stroke.
    Expect: Must not: a third row appears in the CHECKPOINTS list; it still shows 2 rows.
