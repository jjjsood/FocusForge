# Retouch from checkpoints: choose a result as brush source, paint from it, delete it
Start: --open fixtures/mantis-512/frames --fuse-tabs 0 --enter-editor

Use a fresh settings directory, where the source presentation is Image list. The journey builds four results in one tab, in this order: the fuse that starts with the window (Details, ends in the keep question, answered with **Keep**), a second fuse with alignment switched off (no question; its result becomes the edited state), a third fuse with alignment back on and the algorithm "Focus map" (keep question again), and one stroke painted from the Details result and saved as a checkpoint. The order matters: the Images dock lists a result only while it shares the pixel grid of the edited state, so the alignment-off result is the one left out, as the line "1 hidden: different alignment". The CHECKPOINTS list counts rows from 0, newest first (`FFCheckpointRow::area` 0 is the newest). In the Images dock `FFImageRow::area` counts the rows that exist on screen in visible order, starting at the first row under the heading "Results"; the hidden line counts, headings do not, and a row scrolled out of the list is not counted. Selecting a frame scrolls the list, so after step 17 the first result row is out of view and the counts shift by one (steps 19 and 27 say so). `FFSectionHeading::root` counts the headings in view in the same way. `FFMenu::root` 0 is the field Algorithm under FUSE while the toolbar shows **Fuse stack**. The left pane label (`FFCanvas::pane-left`) reads a frame's file name ("00.tif") or "Checkpoint · " with the algorithm and a time, and it is cut at 300 px, so the steps quote only its start. Alt+click is `click-id FFCanvas::pane-right --hold Alt`; the Composite's centre is where the stroke starts, so an Alt+click on it lands on the stroke. The undo arrow, the redo arrow and **Save checkpoint** in the toolbar are read with `buttons AppWindow::editor-toolbar`: enabled or greyed. **Save project** saves a pending stroke as a checkpoint too, so a stroke that was painted and not saved with **Save checkpoint** appears as one more checkpoint after the project is saved.

## Build four results
1. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: (within 120 s) the dialog is gone; the CHECKPOINTS list shows 1 row; the Images dock list starts with the heading "Results" and the count "1", then one row reading "Details" with the pills "BASE" and "UNSAVED".
2. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the toolbar shows **Fuse stack**; the dock shows the tabs **Images** and **Settings**.
3. Click **Settings** (`FFDockTabs::settings-tab`).
   Expect: the dock lists the headings "PREPARE", "FUSE" and "ALIGN".
4. Click the **Enabled** switch under ALIGN (`FFToggle::area` 0).
   Expect: the switch is off; the heading "ALIGN" has no "DEFAULT" badge.
5. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 120 s) no question appears and the window shows the editor; the CHECKPOINTS list shows 2 rows; the Images dock list shows "Results" with the count "1", one row reading "Details", and under it the line "1 hidden: different alignment".
6. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the toolbar shows **Fuse stack**.
7. Click **Settings** (`FFDockTabs::settings-tab`).
   Expect: the dock lists the headings "PREPARE", "FUSE" and "ALIGN"; the **Enabled** switch under ALIGN is off.
8. Click the **Enabled** switch under ALIGN (`FFToggle::area` 0).
   Expect: the switch is on.
9. Click the field **Algorithm** under FUSE (`FFMenu::root` 0).
   Expect: three lines "Details", "Soft transitions" and "Focus map" appear.
10. Click the entry **Focus map**, the last of the three (`FFMenuList::option-area` 2).
   Expect: the list is gone and the field reads "Focus map".
11. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 120 s) a dialog titled "Keep this fused result?" shows the buttons "Discard" and "Keep".
12. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone and the window shows the editor; the CHECKPOINTS list shows 3 rows; the Images dock list shows "Results" with the count "2": the first row "Focus map" with the pills "BASE" and "UNSAVED", the second row "Details" with "UNSAVED" only; the line "1 hidden: different alignment" follows them.
## Paint from a result
13. Click the second result row, "Details" (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) starts "Checkpoint · Details"; the "Details" row is selected, drawn with a light wash and a bar at its left edge; frame 00.tif is not selected.
14. (human only) Move the pointer over the Composite (`FFCanvas::pane-right`).
   Expect: the brush ring is white and shows a small layers glyph at its centre.
15. Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
   Expect: (within 120 s) `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled true.
16. Click **Save checkpoint** (`AppWindow::editor-toolbar`, top right).
   Expect: the CHECKPOINTS list shows 4 rows, the first reading "Retouch on Focus map"; the Images dock list shows "Results" with the count "3": "Focus map" with "· 1 stroke" and the pills "BASE" and "UNSAVED", "Focus map" with "FUSION" and "UNSAVED", and "Details" with "UNSAVED"; the left pane label still starts "Checkpoint · Details"; `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled false.
## The stroke remembers its source
17. Press Down.
   Expect: frame 00.tif is selected and the left pane label (`FFCanvas::pane-left`) reads "00.tif"; no result row has a wash.
18. Alt+click the Composite at its centre (`FFCanvas::pane-right`, `--hold Alt`).
   Expect: the "Details" result row is selected and frame 00.tif is not; the left pane label (`FFCanvas::pane-left`) starts "Checkpoint · Details".
19. Click the line "1 hidden: different alignment" (`FFImageRow::area` 2; the first result row is scrolled out of view, so match 0 is the "Focus map" FUSION row, 1 is "Details", 2 is the hidden line and 3 is frame 00.tif).
   Expect: Must not: the line takes the selection, a frame row becomes selected or the left pane label changes; the "Details" row stays selected and the label still starts "Checkpoint · Details".
## Delete a result that a stroke uses
20. Hover the last row of the CHECKPOINTS list (`FFCheckpointRow::area` 3).
   Expect: `buttons FFDock::checkpoint-panel` lists **Delete checkpoint** (`FFCheckpointRow::trash`) once; no dialog is shown.
21. Click **Delete checkpoint** (`FFCheckpointRow::trash`).
   Expect: a dialog titled "Delete this checkpoint?" opens with the context line "Details · " and a time; its text reads "This checkpoint is the source of 1 brush stroke in another checkpoint." and "It is hidden and kept in the project, so that stroke keeps its pixels. The other 3 checkpoints are untouched, and the stack is not fused again."; its buttons read **Cancel** and **Delete**.
22. Click **Delete** (`AppWindow::checkpoint-delete`).
   Expect: the dialog is gone; the CHECKPOINTS list shows 3 rows, one of them reading "Details"; no row in the Images dock list reads "Details"; the line "1 hidden: different alignment" is still there; the left pane label (`FFCanvas::pane-left`) reads "00.tif".
23. Alt+click the Composite at its centre (`FFCanvas::pane-right`, `--hold Alt`).
   Expect: Must not: a result row is selected or the left pane label starts "Checkpoint · "; frame 00.tif stays selected and the label reads "00.tif".
## Save the project
24. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overview (`AppWindow::projects-overlay`) opens; `buttons AppWindow::projects-overlay` lists **Save project** with enabled true; the project's card reads "8 frames · 3 checkpoints" and carries an "UNSAVED" chip.
25. Click **Save project** (`FFProjectsRail::save-project`).
   Expect: (within 30 s) no file chooser opens; the card still reads "8 frames · 3 checkpoints" and carries no "UNSAVED" chip; the tab strip shows no "UNSAVED" marker.
26. Press Escape.
   Expect: the overview is gone and the Images dock shows the editor list with no pill reading "UNSAVED".
27. Click the heading "8 frames" (`FFSectionHeading::root` 0; the heading "Results" is scrolled out of the list, so match 0 is "8 frames").
   Expect: the frame rows are gone and the list is back at its top: "Results" with the count "2", the first row "Focus map" with "· 1 stroke" and the pill "BASE", the second row "Focus map" with the pill "FUSION", no pill reads "UNSAVED", the line "1 hidden: different alignment", then the heading "8 frames" with a right-pointing arrow.
## Paint over the stroke
28. (human only) Hold Ctrl and roll the mouse wheel up one notch over the Composite (`FFCanvas::pane-right`).
   Expect: the second result row, "Focus map" with "FUSION", is selected and the left pane label starts "Checkpoint · Focus map"; Ctrl+wheel steps through the result rows first, then the frames, and the label follows.
29. (human only) Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
   Expect: the area painted by the first stroke is covered: it shows the "Focus map" result without that stroke.
30. Click the first result row, "Focus map" with "BASE" (`FFImageRow::area` 0).
   Expect: the left pane label (`FFCanvas::pane-left`) starts "Checkpoint · Focus map" and ends "· 1 stroke"; the first result row is selected, drawn with a light wash and a bar at its left edge, and no other row is.
31. (human only) Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
   Expect: the area looks as it did after step 16 again: painting from the base restores it.
