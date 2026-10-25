# Export a fused result: refusal before a fuse, dialog, formats, settings, cancel, Escape, write
Start: --open fixtures/mantis-64/frames
## Export stays unavailable before a kept fuse
1. Click **Export image** (`FFCheckpointPanel::checkpoint-export`) before anything is fused.
   Expect: Must not: a dialog titled "Export image" opens; **Export image** is greyed out (read with `buttons` within `FFDock::checkpoint-panel`: enabled false); the CHECKPOINTS list shows the line "None yet — fuse a stack".
2. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) the dialog "Keep this fused result?" with **Discard** and **Keep** is shown; it has no icon button at its top-right corner (`buttons AppWindow::divergence-warning` does not list **Close dialog**).
3. Click **Export image** (`FFCheckpointPanel::checkpoint-export`) while the "Keep this fused result?" dialog is open.
   Expect: Must not: a dialog titled "Export image" opens; the "Keep this fused result?" dialog is still shown and the CHECKPOINTS list still shows "None yet — fuse a stack".
4. Press Escape while the "Keep this fused result?" dialog is open.
   Expect: Must not: the dialog closes; the "Keep this fused result?" dialog is still shown with **Discard** and **Keep** (`buttons AppWindow::divergence-warning` lists both) and the CHECKPOINTS list still shows "None yet — fuse a stack".
5. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone, the CHECKPOINTS list shows 1 row, the line "None yet — fuse a stack" is gone and **Export image** is enabled (read with `buttons` within `FFDock::checkpoint-panel`).
## Read the dialog
6. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: a dialog titled "Export image" opens with the line "Exporting the unsaved fuse result held in this tab.", a "Format" row with the segments TIFF, PNG and JPEG with TIFF highlighted, the heading "TIFF SETTINGS" over "Bit depth" reading "16-bit" and "Compression" reading "None", no "Quality" row, and a "METADATA" heading; an icon button **Close dialog** sits at the panel's top-right corner (`buttons AppWindow::export-dialog` lists it).
7. Click the "Bit depth" field showing "16-bit" (`FFMenu::root`, match 0; match 1 is "Compression").
   Expect: a list with the options 16-bit and 8-bit opens under the field, with a check mark beside 16-bit.
8. Click the option 8-bit (`FFMenuList::option-area`, match 1).
   Expect: the list is closed and the "Bit depth" field reads "8-bit".
9. Click the switch beside "Metadata" (`FFToggle::area`, match 1; match 0 is the bottom bar's overlay switch).
   Expect: the switch's knob is at its left end (it was at its right end before the click).
10. Click the switch beside "Metadata" (`FFToggle::area`, match 1).
   Expect: the switch's knob is at its right end again.
11. Click the "Compression" field showing "None" (`FFMenu::root`, match 1; match 0 is "Bit depth").
   Expect: a list with the options None, LZW and Deflate opens under the field, with a check mark beside None.
12. Click the option Deflate (`FFMenuList::option-area`, match 2).
   Expect: the list is closed and the "Compression" field reads "Deflate".
## Change the format
13. Click the middle segment **PNG** of the Format row (`FFSegmented::root`, match 0; its centre is the PNG segment).
   Expect: the heading reads "PNG SETTINGS" over "Bit depth" and "Compression" reading "Default"; "TIFF SETTINGS" and "Quality" are not shown.
14. Click **JPEG** in the Format row (`FFSegmented::segment` 2).
   Expect: the heading reads "JPEG SETTINGS" over a "Quality" row with a slider whose value reads "95" and a "Chroma subsampling" row with the segments 4:4:4 and 4:2:0; "Bit depth" and "Compression" are not shown; `buttons AppWindow::export-dialog` lists **JPEG** with checked=true and **PNG** with checked=false.
15. Click **4:4:4** in the Chroma subsampling row (`FFSegmented::segment` 3).
   Expect: `buttons AppWindow::export-dialog` lists **4:4:4** with checked=true and **4:2:0** with checked=false.
16. Click **4:2:0** in the Chroma subsampling row (`FFSegmented::segment` 4).
   Expect: `buttons AppWindow::export-dialog` lists **4:2:0** with checked=true and **4:4:4** with checked=false.
17. (human only) Drag the Quality slider in the dialog to its left end.
   Expect: the value beside the slider reads "1".
## Cancel and Escape write nothing
18. Click **Cancel** (`AppWindow::export-dialog`).
   Expect: Must not: a heading "Writing image…" or a file chooser is shown; the "Export image" dialog is gone, the CHECKPOINTS list still shows 1 row and the bottom bar still reads "fused" followed by a time.
19. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: the dialog "Export image" opens with the heading "TIFF SETTINGS", the "Compression" field reading "None" and the "Metadata" switch's knob at its right end; "JPEG SETTINGS" and "Deflate" are not shown.
20. Click the "Compression" field showing "None" (`FFMenu::root`, match 1).
   Expect: a list with the options None, LZW and Deflate opens under the field, with a check mark beside None.
21. Press Escape while the list is open.
   Expect: Must not: the dialog closes; the list is closed, the "Export image" dialog is still shown with the heading "TIFF SETTINGS" and the "Compression" field reading "None".
22. Press Escape again.
   Expect: Must not: a heading "Writing image…" or a file chooser is shown; the "Export image" dialog is gone, the CHECKPOINTS list still shows 1 row and the bottom bar still reads "fused" followed by a time.
## What the dialog says it exports
23. Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
   Expect: **Save checkpoint** is no longer greyed out (read with `buttons` within `AppWindow::editor-toolbar`: enabled true).
24. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: the dialog "Export image" opens with the line "Exporting your current unsaved retouch edits."; the line "Exporting the unsaved fuse result held in this tab." is not shown.
25. Click **Close dialog** (`AppWindow::export-dialog`), the icon button at the panel's top-right corner.
   Expect: Must not: a heading "Writing image…" or a file chooser is shown; the "Export image" dialog is gone and the CHECKPOINTS list still shows 1 row.
26. Click **Save checkpoint** (`AppWindow::editor-toolbar`).
   Expect: the CHECKPOINTS list shows 2 rows; **Save checkpoint** is greyed out (read with `buttons` within `AppWindow::editor-toolbar`: enabled false).
27. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: the dialog "Export image" opens with the context line "frames · retouch checkpoint on Details" followed by a date and time and the line "Exporting the unsaved retouch checkpoint held in this tab."; the lines "Exporting your current unsaved retouch edits." and "Exporting the saved checkpoint you selected." are not shown.
28. Click **Cancel** (`AppWindow::export-dialog`).
   Expect: the "Export image" dialog is gone and the CHECKPOINTS list shows 2 rows.
## Escape on the delete question deletes nothing
29. Hover the first row of the CHECKPOINTS list (`FFCheckpointRow::area` 0).
   Expect: a red trash icon shows at the right edge of the first row; `buttons FFDock::checkpoint-panel` lists **Delete checkpoint** with id `FFCheckpointRow::trash`; no dialog is shown.
30. Click **Delete checkpoint** (`FFCheckpointRow::trash`).
   Expect: a dialog titled "Delete this checkpoint?" opens with the buttons **Cancel** and **Delete**.
31. Press Escape while the "Delete this checkpoint?" dialog is open.
   Expect: Must not: a row disappears; the dialog is gone and the CHECKPOINTS list still shows 2 rows.
## Export writes the file
32. Click **Export image** (`FFCheckpointPanel::checkpoint-export`).
   Expect: the dialog "Export image" opens with the heading "TIFF SETTINGS".
33. Click **Export** (`AppWindow::export-dialog`).
   Expect: (within 60 s) no file chooser opens and the "Export image" dialog is gone; the CHECKPOINTS list still shows 2 rows.
34. (human only) Click **Export image**, then **Export**, on a stack large enough that the heading "Writing image…" stays visible for a moment, and press Escape while it shows.
   Expect: the "Export image" dialog is gone at once, as with **Close dialog**, and the exported file is still written to the save folder.
35. (human only) Start the window by hand without `--save-folder`, keep a fuse, click **Export image** and **Export** so the system save dialog opens, then press Escape inside it.
   Expect: the system save dialog is gone and the "Export image" dialog is still shown.
