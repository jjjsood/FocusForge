# Editor source list: Results and Frames in Image list, Large previews and List
Start: --open fixtures/mantis-512/frames --fuse-tabs 0 --enter-editor

Use a fresh settings directory, where the source presentation is Image list. The fuse that starts with the window ends in the keep question; the journey answers **Keep** and the window then shows the editor. The journey fuses a second time with alignment switched off; that fuse ends in the editor without a question, and the first result no longer shares the new result's pixel grid, so it is listed only as a hidden count. Then it saves one stroke as a third result. Nothing is saved to a project file, so every result is marked UNSAVED. The editor list holds the heading "Results" (with a count), the result rows, a line "N hidden: reason" when results were left out, the heading "8 frames" and the frame rows; headings are not rows, the hidden line is one. `FFImageRow::area` counts the rows on screen in visible order, starting at the first row under the heading "Results"; `FFSectionHeading::root` counts the headings in visible order only while the list has not been scrolled (0 is "Results", 1 is "8 frames"), so the heading steps come before any step that moves the selection to a row lower in the list. `FFMenu::root` counts over the whole window: match 0 is the blend mode "Normal" in the editor toolbar, match 1 is Presentation in the open Settings window. The Settings window opens on Performance when it is reopened from the icon bar (bottom-left): click **Appearance** each time. Result previews are drawn one after another; the steps that read them allow a minute. After a presentation change the list scrolls so that the selected frame 00.tif stays in view: the heading "Results" and the top of the first result row may then be cut off, and a heading scrolled out of the list has no `FFSectionHeading::root` match, so the heading "8 frames" is then match 0. The tooltip on a row is a native one that the driver does not raise: the hover step is a person's.

## One result in the list
1. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: (within 120 s) the dialog is gone; the CHECKPOINTS list shows 1 row; the Images dock list starts with the heading "Results" and the count "1", then one row reading "Details" and a time, with the pills "BASE" and "UNSAVED" and a preview picture, then the heading "8 frames"; no line reads "hidden" and no text in the list reads "group".
## A second result with a different alignment
2. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the toolbar shows **Fuse stack**; the dock shows the tabs **Images** and **Settings**.
3. Click **Settings** (`FFDockTabs::settings-tab`).
   Expect: the dock lists the headings "PREPARE", "FUSE" and "ALIGN" (the heading "FINISH" lies below the visible part of the dock).
4. Click the **Enabled** switch under ALIGN (`FFToggle::area` 0).
   Expect: the switch is off; the ALIGN heading has no "DEFAULT" badge.
5. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 120 s) no question appears and the window shows the editor; the CHECKPOINTS list shows 2 rows; the Images dock list shows "Results" with the count "1", one row reading "Details" with the pills "BASE" and "UNSAVED", and under it the line "1 hidden: different alignment".
## Save a stroke as a third result
6. Drag across the Composite pane from its centre to window point 1000,400 (`FFCanvas::pane-right`).
   Expect: (within 120 s) `buttons AppWindow::editor-toolbar` lists **Save checkpoint** with enabled true.
7. Click **Save checkpoint** (`AppWindow::editor-toolbar`, top right): drive it with `click "Save checkpoint"` alone, without the container id, which finds no button.
   Expect: the CHECKPOINTS list shows 3 rows; the list shows "Results" with the count "2"; the first result row reads "Details" and "· 1 stroke" with the pills "BASE" and "UNSAVED", the second reads "Details" with the pills "FUSION" and "UNSAVED", and the line "1 hidden: different alignment" is still under them.
## Collapse and expand "Results"
8. Click the heading "Results" (`FFSectionHeading::root` 0).
   Expect: the result rows and the hidden line are gone; the heading still reads "Results" with "2" and shows a right-pointing arrow; the heading "8 frames" and the frame rows stay.
9. Press Up.
   Expect: Must not: any row other than frame 00.tif is selected; frame 00.tif stays selected and the left pane label (`FFCanvas::pane-left`) reads "00.tif".
10. Click the heading "Results" (`FFSectionHeading::root` 0).
   Expect: both result rows and the line "1 hidden: different alignment" are back; the heading shows a down-pointing arrow.
## Large previews
11. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: the Settings window opens.
12. Click **Appearance** (`FFNavRailEntry::area` 1).
   Expect: the field Presentation reads "Image list".
13. Click **Presentation** (`FFMenu::root` 1; match 0 is the blend mode "Normal").
   Expect: the three lines "List", "Image list" and "Large previews" appear.
14. Click the entry **Large previews**, the last of the three (`FFMenuList::option-area` 2).
   Expect: the list is gone and the Presentation field reads "Large previews".
15. Click **Close settings** (`FFSettingsModal::close`).
   Expect: (within 60 s) every result row in view is a wide preview picture (`FFImageRow::large-identity`) with one line under it holding a layers icon, the text ("Details", a time, "· 1 stroke" on the first row) and its pills, and no picture reads "Loading…"; the second result row (FUSION, UNSAVED) is always in view, the first may be scrolled out of the list; the line "1 hidden: different alignment" follows them, then the heading "8 frames".
## List
16. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: the Settings window opens.
17. Click **Appearance** (`FFNavRailEntry::area` 1).
   Expect: the field Presentation reads "Large previews".
18. Click **Presentation** (`FFMenu::root` 1; match 0 is the blend mode "Normal").
   Expect: the three lines "List", "Image list" and "Large previews" appear.
19. Click the entry **List**, the first of the three (`FFMenuList::option-area` 0).
   Expect: the list is gone and the Presentation field reads "List".
20. Click **Close settings** (`FFSettingsModal::close`).
   Expect: the list shows "Results" with "2", then two one-line rows with a layers icon, "Details", the start of a time and the pills; a row whose text does not fit ends in an ellipsis, so "· 1 stroke" may be cut off here (it is read in the Image list and Large previews steps); then the line "1 hidden: different alignment", then "8 frames" and the frames 00.tif and below as compact lines; no preview pictures are drawn.
## Image list
21. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: the Settings window opens.
22. Click **Appearance** (`FFNavRailEntry::area` 1).
   Expect: the field Presentation reads "List".
23. Click **Presentation** (`FFMenu::root` 1; match 0 is the blend mode "Normal").
   Expect: the three lines "List", "Image list" and "Large previews" appear.
24. Click the entry **Image list**, the middle one of the three (`FFMenuList::option-area` 1).
   Expect: the list is gone and the Presentation field reads "Image list".
25. Click **Close settings** (`FFSettingsModal::close`).
   Expect: (within 60 s) two result rows each with a preview picture (no "Loading…"), a layers icon and pills at the top right of the row text (the first row's top edge, with its layers icon and BASE pill, may be cut off by the scroll); "Details" with "· 1 stroke" and a time on the first row, "Details" and a time on the second; the line "1 hidden: different alignment"; then the heading "8 frames" and the frame 00.tif with its preview picture.
## Choose a brush source
26. (human only) Hover the first result row (`FFImageRow::area` 0).
   Expect: a tooltip reads "Use as brush source"; the row shows no eye or trash icon.
27. Click the second result row (`FFImageRow::area` 1).
   Expect: Must not: a frame row stays selected, or the CHECKPOINTS list selection moves off its top row; only the second result row is selected, drawn with a light wash and a bar at its left edge, and the left pane label (`FFCanvas::pane-left`) starts "Checkpoint · Details".
28. Click the first frame row (`FFImageRow::area` 3).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "00.tif"; the frame row is selected and no result row has a wash; the frame row shows no eye or trash icon.
## Step through one list
29. Press Up.
   Expect: the second result row is selected (light wash and bar) and frame 00.tif is not; the heading "8 frames" and the hidden line were passed over.
30. Press Down.
   Expect: frame 00.tif is selected again, the left pane label (`FFCanvas::pane-left`) reads "00.tif" and no result row has a wash.
31. (human only) Roll the mouse wheel up one notch while the pointer is over frame 00.tif (`FFImageRow::area` 3).
   Expect: the second result row is selected, as after Press Up; with the pointer over the hidden line the wheel scrolls the list instead.
32. (human only) Hold Ctrl and roll the mouse wheel down one notch over the Composite pane (`FFCanvas::pane-right`).
   Expect: frame 00.tif is selected again, as after Press Down.
33. (human only) Hold Shift and roll the mouse wheel up one notch over the Composite pane (`FFCanvas::pane-right`).
   Expect: the brush Size value in the toolbar changes; the selected row does not.
34. Click the heading "8 frames" (`FFSectionHeading::root` 0 when "Results" is scrolled out of the list, else 1).
   Expect: the frame rows are gone and the list is back at its top: "Results" with "2", two result rows each with a preview picture and pills, the line "1 hidden: different alignment", then the heading "8 frames" with a right-pointing arrow.
