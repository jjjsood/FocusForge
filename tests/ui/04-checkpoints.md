# Fuse again, restore and delete checkpoints
Start: --open fixtures/mantis-64/frames --fuse-tabs 0,0
## Fuse twice at launch
1. Hover the bottom bar status (`FFBottomBar::status-text`) while the first "Keep this fused result?" question is shown.
   Expect: Must not: a second run starts; the dialog "Keep this fused result?" is still shown, the bottom bar reads "No job running" and the CHECKPOINTS list reads "None yet — fuse a stack".
2. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the second run starts on its own and a dialog titled "Keep this fused result?" is shown again; the CHECKPOINTS list shows 1 row (within 60 s).
3. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone; the bottom bar reads "fused" followed by a time; the CHECKPOINTS list shows 2 rows, each with an algorithm name above a HH:MM:SS time; the toolbar row shows "Size" and "Soft" (within 60 s).
## Select a checkpoint
4. Click the second row of the CHECKPOINTS list (`FFCheckpointRow::area` 1).
   Expect: the second row has a raised background and a vertical accent bar at its left edge; the first row has neither; the right pane (`FFCanvas::pane-right`) shows a filled composite; no dialog is shown.
5. Click the first row of the CHECKPOINTS list (`FFCheckpointRow::area` 0).
   Expect: the accent bar and raised background are on the first row and gone from the second; the list still shows 2 rows.
## Fuse again over a stroke
6. Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
   Expect: (within 60 s) `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled true; the first row of the CHECKPOINTS list still has the raised background and the accent bar.
7. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the toolbar row shows the **Fuse stack** button at its left and no longer shows "Size" or "Soft".
8. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: a dialog titled "Save your edits first?" appears with the line "Fusing frames" and the text "1 unsaved brush stroke will be lost."; its buttons read "Cancel", "Save and fuse" and "Fuse anyway", beside a **Close dialog** icon button at its top-right corner.
9. Click **Cancel** (`AppWindow::close-guard`).
   Expect: Must not: the stack is fused again; the dialog is gone, the bottom bar still reads "fused" followed by a time, no "Keep this fused result?" dialog is shown and the CHECKPOINTS list still shows 2 rows.
10. Click **EDITOR** (`FFModeSwitch::editor-half`).
    Expect: the toolbar row shows "Size", "Soft" and **Save checkpoint**; `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled true (the stroke is still there).
11. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the toolbar row shows the **Fuse stack** button and no longer shows "Size" or "Soft".
12. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: the dialog titled "Save your edits first?" appears again with the buttons "Cancel", "Save and fuse" and "Fuse anyway" and the **Close dialog** icon button at its top-right corner.
13. Click **Fuse anyway** (`AppWindow::close-guard`).
    Expect: the dialog "Save your edits first?" is gone and a new dialog "Keep this fused result?" is shown (within 60 s).
14. Click **Discard** (`AppWindow::divergence-warning`).
    Expect: Must not: a third row is added; the dialog is gone and the CHECKPOINTS list still shows 2 rows.
15. Click **EDITOR** (`FFModeSwitch::editor-half`).
    Expect: the toolbar row shows "Size", "Soft" and **Save checkpoint**; `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled true (a discarded run keeps the stroke).
## Restore over a stroke
16. Click the second row of the CHECKPOINTS list (`FFCheckpointRow::area` 1).
    Expect: a dialog titled "Save this state first?" appears with a line starting "Restoring" and ending in a HH:MM:SS time, and the text "1 brush stroke has happened since the last checkpoint."; its buttons read "Cancel", "Save first, then restore" and "Discard and restore", beside a **Close dialog** icon button at its top-right corner.
17. Click **Cancel** (`AppWindow::close-guard`).
    Expect: Must not: the second row becomes the selected one; the dialog is gone, the first row of the CHECKPOINTS list still has the raised background and the accent bar, the second row has neither, and `buttons AppWindow::editor-toolbar` still lists **Undo** with enabled true.
18. Click the second row of the CHECKPOINTS list (`FFCheckpointRow::area` 1).
    Expect: the dialog titled "Save this state first?" appears again with the buttons "Cancel", "Save first, then restore" and "Discard and restore".
19. Click **Close dialog** (`AppWindow::close-guard`), the icon button at the dialog's top-right corner.
    Expect: Must not: the second row becomes the selected one; the dialog is gone, the first row of the CHECKPOINTS list still has the raised background and the accent bar, the second row has neither, and `buttons AppWindow::editor-toolbar` still lists **Undo** with enabled true.
20. Click the second row of the CHECKPOINTS list (`FFCheckpointRow::area` 1).
    Expect: the dialog titled "Save this state first?" appears again with the buttons "Cancel", "Save first, then restore" and "Discard and restore".
21. Click **Discard and restore** (`AppWindow::close-guard`).
    Expect: the dialog is gone; the second row of the CHECKPOINTS list has the raised background and the accent bar and the first row has neither; the list still shows 2 rows; `buttons AppWindow::editor-toolbar` lists **Undo** with enabled false.
22. Click the first row of the CHECKPOINTS list (`FFCheckpointRow::area` 0).
    Expect: Must not: a "Save this state first?" dialog appears; no dialog is shown, the first row has the raised background and the accent bar and the second row has neither.
## Delete a checkpoint
23. Hover the first row of the CHECKPOINTS list (`FFCheckpointRow::area` 0).
    Expect: a red trash icon shows at the right edge of the first row; the second row shows none; `buttons FFDock::checkpoint-panel` lists **Delete checkpoint** with id `FFCheckpointRow::trash` (once, for the first row); the list still shows 2 rows and no dialog is shown.
24. Click **Delete checkpoint** (`FFCheckpointRow::trash`).
    Expect: a dialog titled "Delete this checkpoint?" opens with the first row's "algorithm · HH:MM:SS" as its context line and the line "The other checkpoint is untouched, and the stack is not fused again."; its buttons read **Cancel** and **Delete**, beside a **Close dialog** icon button at its top-right corner.
25. Click **Cancel** (`AppWindow::checkpoint-delete`).
    Expect: Must not: a row disappears; the dialog is gone and the CHECKPOINTS list still shows 2 rows.
26. Hover the first row of the CHECKPOINTS list again (`FFCheckpointRow::area` 0; the click on **Cancel** moved the pointer off the row).
    Expect: `buttons FFDock::checkpoint-panel` lists **Delete checkpoint** with id `FFCheckpointRow::trash`; no dialog is shown.
27. Click **Delete checkpoint** (`FFCheckpointRow::trash`).
    Expect: the "Delete this checkpoint?" dialog opens again.
28. Click **Close dialog** (`AppWindow::checkpoint-delete`), the icon button at the dialog's top-right corner.
    Expect: Must not: a row disappears; the dialog is gone and the CHECKPOINTS list still shows 2 rows.
29. Hover the first row of the CHECKPOINTS list again (`FFCheckpointRow::area` 0; the click on **Close dialog** moved the pointer off the row).
    Expect: `buttons FFDock::checkpoint-panel` lists **Delete checkpoint** with id `FFCheckpointRow::trash`; no dialog is shown.
30. Click **Delete checkpoint** (`FFCheckpointRow::trash`).
    Expect: the "Delete this checkpoint?" dialog opens again.
31. Click **Delete** (`AppWindow::checkpoint-delete`).
    Expect: the dialog is gone; the CHECKPOINTS list shows 1 row and no "None yet — fuse a stack" line; the remaining row has the raised background and the accent bar.
32. Hover the remaining row of the CHECKPOINTS list (`FFCheckpointRow::area` 0).
    Expect: Must not: a red trash icon appears on it; `buttons FFDock::checkpoint-panel` does not list **Delete checkpoint**.
33. Click the first row of the CHECKPOINTS list (`FFCheckpointRow::area` 0).
    Expect: no dialog is shown; the first row has the raised background and the accent bar.
