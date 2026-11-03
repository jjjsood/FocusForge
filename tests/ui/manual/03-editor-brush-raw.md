# Retouch a fused camera-RAW stack, save, export, close and reopen
Start: --open .scratch/raw-gui/frames
Before the first step, copy three real camera DNG files into the folder that `--open` names (they are about 90 MB each and never part of the repository):
`mkdir -p .scratch/raw-gui/frames`
`cp .scratch/raw-f32/real/dng/a1.dng .scratch/raw-gui/frames/frame-1.dng`
`cp .scratch/raw-f32/real/dng/a2.dng .scratch/raw-gui/frames/frame-2.dng`
`cp .scratch/raw-f32/real/dng/a1.dng .scratch/raw-gui/frames/frame-3.dng`
The window then shows one project named "frame-1-3" with 3 frames. A fuse takes a few minutes and ends without a "Keep this fused result?" question.
## Fuse the RAW stack
1. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: the toolbar row shows **Pause** and **Cancel** and no longer shows **Fuse stack**; (within 300 s) the bottom bar reads "fused" followed by a time, the dialog "Keep this fused result?" is not shown, the toolbar row shows the captions "Size" and "Soft", the Composite pane (`FFCanvas::pane-right`) shows a picture of a bird on a branch in front of a pale disc, the same scene as the left pane (`FFCanvas::pane-left`) and filling the same area, and the CHECKPOINTS list shows 1 row, "Details".
2. Click **Undo** (`FFEditorToolbar::undo`) before any stroke.
   Expect: Must not: **Redo** becomes enabled; `buttons AppWindow::editor-toolbar` lists **Undo**, **Redo** and **Save checkpoint** with enabled false.
3. Click **Save checkpoint** (`AppWindow::editor-toolbar`) before any stroke.
   Expect: Must not: the CHECKPOINTS list grows; it still shows 1 row.
## Paint from frame 2 over the bright disc
4. Click the second frame row (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "frame-2.dng"; the dock list shows 3 rows.
5. Drag the Size slider thumb to window point 170,56 (`FFSlider::drag-area` 0).
   Expect: the text beside the Size slider reads "2293"; the text beside the Soft slider still reads "30%".
6. Drag across the Composite pane from its centre to window point 830,370, across the pale disc (`FFCanvas::pane-right`).
   Expect: (within 120 s) `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled true and **Redo** with enabled false.
7. Click **Undo** (`FFEditorToolbar::undo`).
   Expect: `buttons AppWindow::editor-toolbar` lists **Undo** with enabled false and **Redo** with enabled true.
8. Click **Redo** (`FFEditorToolbar::redo`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Redo** with enabled false and **Undo** with enabled true.
## Save a checkpoint
9. Click **Save checkpoint** (`AppWindow::editor-toolbar`).
    Expect: the CHECKPOINTS list shows 2 rows; `buttons AppWindow::editor-toolbar` lists **Save checkpoint**, **Undo** and **Redo** with enabled false.
10. Click **Save checkpoint** (`AppWindow::editor-toolbar`) again without a new stroke.
    Expect: Must not: a third row appears in the CHECKPOINTS list; it still shows 2 rows.
## Export a 16-bit TIFF
11. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
    Expect: a dialog titled "Export image" opens with the line "Exporting the unsaved retouch checkpoint held in this tab.", the context line "frame-1-3 · retouch checkpoint on Details" followed by a date and time, **TIFF** highlighted and "Bit depth" reading "16-bit".
12. Click **Cancel** (`AppWindow::export-dialog`).
    Expect: Must not: a heading "Writing image…" or a file chooser is shown; the "Export image" dialog is gone and the CHECKPOINTS list shows 2 rows.
13. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
    Expect: the dialog "Export image" opens with "Bit depth" reading "16-bit".
14. Click **Export** (`AppWindow::export-dialog`).
    Expect: (within 300 s) no file chooser opens and the "Export image" dialog is gone; the CHECKPOINTS list still shows 2 rows.
15. (human only) Look in the save folder (`.scratch/ui/save` when started by hand as in the README; rename the file to `.scratch/raw-gui/edited.tif` if you keep it).
    Expect: a file `frame-1-3.tif` of about 190 MB is there and `tiffinfo` on it reports "Bits/Sample: 16" and the size 6954 x 4634.
## Save the project, close it and reopen it
16. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overlay (`AppWindow::projects-overlay`) shows one card "frame-1-3" reading "3 frames · 2 checkpoints" with an "UNSAVED" chip.
17. Click **Save project** (`FFProjectsRail::save-project`).
    Expect: (within 120 s) the "UNSAVED" chip is gone from the card, one row "frame-1-3" sits under "RECENT" and no file chooser opens.
18. Click **Save project** (`FFProjectsRail::save-project`) again with nothing new to write.
    Expect: Must not: a file chooser or dialog opens; `buttons AppWindow::projects-overlay` lists **Save project** with enabled false.
19. Click **Close project** (`FFProjectsRail::close-project`).
    Expect: Must not: a dialog titled "Save this state first?" appears; the count reads "0 open", the text "No projects open" is shown and the row "frame-1-3" stays under "RECENT".
20. Click the RECENT row "frame-1-3" (`FFProjectsRail::recent-row` 0).
    Expect: (within 120 s) the overlay is gone and the tab strip shows one tab titled "frame-1-3" with no "UNSAVED" marker.
21. Click **Fit** (`FFZoomCluster::zoom-fit`).
    Expect: the Composite pane (`FFCanvas::pane-right`) shows the same picture of the bird in front of the pale disc; the CHECKPOINTS list shows 2 rows, "Details" over a time, newest first.
## Enter the editor again and paint once more
22. Click **EDITOR** (`FFModeSwitch::editor-half`).
    Expect: the toolbar row shows "Size", "Soft" and **Save checkpoint**; `buttons AppWindow::editor-toolbar` lists **Undo**, **Redo** and **Save checkpoint** with enabled false; the CHECKPOINTS list shows 2 rows.
23. Click the second frame row (`FFImageRow::area` 1).
    Expect: the left pane label (`FFCanvas::pane-left`) reads "frame-2.dng".
24. Drag across the Composite pane from its centre to window point 830,370 (`FFCanvas::pane-right`).
    Expect: (within 120 s) `buttons AppWindow::editor-toolbar` lists **Undo** and **Save checkpoint** with enabled true and **Redo** with enabled false.
25. Click **Undo** (`FFEditorToolbar::undo`).
    Expect: `buttons AppWindow::editor-toolbar` lists **Undo** with enabled false and **Redo** with enabled true.
26. Click **Undo** (`FFEditorToolbar::undo`) again while it is greyed out.
    Expect: Must not: **Undo** becomes enabled; `buttons AppWindow::editor-toolbar` still lists **Undo** with enabled false and **Redo** with enabled true.
27. (human only) Look at the Composite, zoomed to 100 %, around the pale disc and the bird.
    Expect: the disc fades smoothly into the sky with no bands, no black or striped blocks and no flat white patches; the window answers clicks.
