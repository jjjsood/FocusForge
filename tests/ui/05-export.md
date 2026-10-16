# Export a fused result: refusal before a fuse, dialog, formats, settings, cancel
Start: --open fixtures/mantis-64/frames
## Export stays unavailable before a kept fuse
1. Click **Export image** (`FFCheckpointPanel::checkpoint-export`) before anything is fused.
   Expect: Must not: a dialog titled "Export image" opens; **Export image** is greyed out (read with `buttons` within `FFDock::checkpoint-panel`: enabled false); the CHECKPOINTS list shows the line "None yet — fuse a stack".
2. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) the dialog "Keep this fused result?" with **Discard** and **Keep** is shown.
3. Click **Export image** (`FFCheckpointPanel::checkpoint-export`) while the "Keep this fused result?" dialog is open.
   Expect: Must not: a dialog titled "Export image" opens; the "Keep this fused result?" dialog is still shown and the CHECKPOINTS list still shows "None yet — fuse a stack".
4. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone, the CHECKPOINTS list shows 1 row, the line "None yet — fuse a stack" is gone and **Export image** is enabled (read with `buttons` within `FFDock::checkpoint-panel`).
## Read the dialog
5. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: a dialog titled "Export image" opens with the line "Exporting the unsaved fuse result held in this tab.", a "Format" row with the segments TIFF, PNG and JPEG with TIFF highlighted, the heading "TIFF SETTINGS" over "Bit depth" reading "16-bit" and "Compression" reading "None", no "Quality" row, and a "METADATA" heading.
6. Click the "Bit depth" field showing "16-bit" (`FFMenu::area`, match 0; match 1 is "Compression").
   Expect: a list with the options 16-bit and 8-bit opens under the field, with a check mark beside 16-bit.
7. Click the option 8-bit (`FFMenuList::option-area`, match 1).
   Expect: the list is closed and the "Bit depth" field reads "8-bit".
8. Click the switch beside "Metadata" (`FFToggle::area`, match 1; match 0 is the bottom bar's overlay switch).
   Expect: the switch's knob is at its left end (it was at its right end before the click).
9. Click the switch beside "Metadata" (`FFToggle::area`, match 1).
   Expect: the switch's knob is at its right end again.
10. Click the "Compression" field showing "None" (`FFMenu::area`, match 1; match 0 is "Bit depth").
   Expect: a list with the options None, LZW and Deflate opens under the field, with a check mark beside None.
11. Click the option Deflate (`FFMenuList::option-area`, match 2).
   Expect: the list is closed and the "Compression" field reads "Deflate".
## Change the format
12. Click the middle segment **PNG** of the Format row (`FFSegmented::root`, match 0; its centre is the PNG segment).
   Expect: the heading reads "PNG SETTINGS" over "Bit depth" and "Compression" reading "Default"; "TIFF SETTINGS" and "Quality" are not shown.
13. (human only) Click **JPEG** in the Format row (the right segment; the segments have no id).
   Expect: the heading reads "JPEG SETTINGS" over a "Quality" row with a slider whose value reads "95" and a "Chroma subsampling" row with the segments 4:4:4 and 4:2:0; "Bit depth" and "Compression" are not shown.
14. (human only) Drag the Quality slider in the dialog to its left end.
   Expect: the value beside the slider reads "1".
## Cancel writes nothing
15. Click **Cancel** (`AppWindow::export-dialog`).
   Expect: Must not: a heading "Writing image…" or a file chooser is shown; the "Export image" dialog is gone, the CHECKPOINTS list still shows 1 row and the bottom bar still reads "fused" followed by a time.
16. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: the dialog "Export image" opens with the heading "TIFF SETTINGS", the "Compression" field reading "None" and the "Metadata" switch's knob at its right end; "JPEG SETTINGS" and "Deflate" are not shown.
17. Click **Cancel** (`AppWindow::export-dialog`).
   Expect: the "Export image" dialog is gone.
## What the dialog says it exports
18. Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
   Expect: **Save checkpoint** is no longer greyed out (read with `buttons` within `AppWindow::editor-toolbar`: enabled true).
19. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: the dialog "Export image" opens with the line "Exporting your current unsaved retouch edits."; the line "Exporting the unsaved fuse result held in this tab." is not shown.
20. Click **Cancel** (`AppWindow::export-dialog`).
   Expect: the "Export image" dialog is gone.
21. Click **Save checkpoint** (`AppWindow::editor-toolbar`).
   Expect: the CHECKPOINTS list shows 2 rows; **Save checkpoint** is greyed out (read with `buttons` within `AppWindow::editor-toolbar`: enabled false).
22. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: the dialog "Export image" opens with the context line "frames · unsaved edits on Details" followed by a date and time and the line "Exporting your current unsaved retouch edits." (the checkpoint lives only in this tab until the project is saved); the line "Exporting the saved checkpoint you selected." is not shown.
23. Click **Cancel** (`AppWindow::export-dialog`).
   Expect: the "Export image" dialog is gone and the CHECKPOINTS list shows 2 rows.
