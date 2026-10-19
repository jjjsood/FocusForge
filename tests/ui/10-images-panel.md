# Manage the frame list before and after a checkpoint
Start: --open fixtures/mantis-512/frames --settings-section appearance
## Compact list
1. Click **Presentation** (`FFMenu::root`).
   Expect: a list opens under the field with the lines "List", "Image list" and "Large previews"; "Image list" is checked.
2. Click **List** (`FFMenuList::option-area` 0).
   Expect: the list is gone and the Presentation field reads "List".
3. Click **Close settings** (`FFSettingsModal::close`).
   Expect: the Settings window is gone; the Images dock lists 00.tif to 07.tif one compact line each, with no preview images.
## Header and selection
4. Click **Settings** (`FFDockTabs::settings-tab`).
   Expect: the dock shows a list of parameter rows; the text "8 frames" is not visible.
5. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the dock header reads "8 frames · 0 excluded"; the list shows the rows 00.tif to 07.tif; two small icon buttons (a folder with a plus, and up/down arrows) sit at the right end of the header.
6. Click the eighth frame row (`FFImageRow::area` 7).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "07.tif"; row 7 shows an eye icon and a red trash icon in place of the capture time.
## Exclude and remove before the first checkpoint
7. Click **Exclude**, the eye icon (`FFImageRow::list-exclude-button` 0).
   Expect: the dock header reads "8 frames · 1 excluded"; the name "07.tif" is drawn dimmer than "00.tif" and its icon is a crossed-out eye.
8. Click **Include**, the crossed-out eye icon (`FFImageRow::list-exclude-button` 0).
   Expect: the dock header reads "8 frames · 0 excluded"; the name "07.tif" is drawn as bright as "00.tif" and its icon is an open eye.
9. Click **Remove**, the red trash icon (`FFImageRow::list-remove-button` 0) on the eighth row.
   Expect: no dialog opens; the dock header reads "7 frames · 0 excluded" and the last row is "06.tif".
10. (human only) Click **Add images**, the folder with a plus at the right end of the header (`FFImagesPanel::add-button`).
   Expect: the system file dialog opens.
11. (human only) In the file dialog choose `fixtures/mantis-512/frames/07.tif` and confirm.
   Expect: the file dialog closes; the dock header reads "8 frames · 0 excluded" and the last row is "07.tif".
## Sort and reorder
12. Click **Sort** (`FFImagesPanel::sort-button`).
   Expect: a menu opens under the header with two lines, "Name" and "Capture time", and no check mark on either.
13. Click "Capture time" (`FFMenuList::option-area` 1).
   Expect: the menu is gone and no dialog opens.
14. Click **Sort** (`FFImagesPanel::sort-button`).
   Expect: the menu opens with a check mark (✓) on the "Capture time" line and none on the "Name" line.
15. Click "Name" (`FFMenuList::option-area` 0).
   Expect: the menu is gone, no dialog opens, and the first three rows read 00.tif, 01.tif, 02.tif from top to bottom.
16. (human only) Press on the file name of the first row (`FFImageRow::list-name-slot` 0), drag down by one row height and release.
   Expect: the first three rows read 01.tif, 00.tif, 02.tif from top to bottom; open the sort menu (`FFImagesPanel::sort-button`) and neither line has a check mark; close it by pressing the sort button again.
17. Click **Sort** (`FFImagesPanel::sort-button`).
   Expect: the menu opens with the lines "Name" and "Capture time" and the "Capture time" line has no check mark.
18. Click "Name" (`FFMenuList::option-area` 0).
   Expect: the menu is gone and the first three rows read 00.tif, 01.tif, 02.tif from top to bottom.
## Fuse, then the read-only list in the editor
19. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: the "Keep this fused result?" dialog is shown (within 60 s).
20. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone; the toolbar row shows the captions "Size" and "Soft"; the CHECKPOINTS list shows 1 row (within 60 s).
21. Click the second frame row (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif"; the dock header ends with "frames" and shows no add or sort icon at its right end; row 1 shows its position "2" at the right end and no eye or trash icon.
## Exclude and remove with a checkpoint
22. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the toolbar shows the **Fuse stack** button; the dock header ends with "· 0 excluded" and the add and sort icons are shown at its right end.
23. Click the second frame row (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif"; row 1 shows the eye icon and the red trash icon.
24. Click **Exclude**, the eye icon (`FFImageRow::list-exclude-button` 0).
   Expect: no dialog opens and the dock header ends with "· 1 excluded".
25. Click **Include**, the crossed-out eye icon (`FFImageRow::list-exclude-button` 0).
   Expect: no dialog opens and the dock header ends with "· 0 excluded".
26. Click **Remove**, the red trash icon (`FFImageRow::list-remove-button` 0) on the second row.
   Expect: a dialog titled "Remove and fuse again?" opens with the line "The whole stack is fused from scratch. This project already holds 1 checkpoint, and confirming keeps every one of them." and the buttons **Cancel**, **Exclude instead** and **Remove and re-fuse**.
27. Click **Cancel** (`AppWindow::frame-warning`).
   Expect: Must not: the "01.tif" row is removed or a second checkpoint is added; the dialog is gone, the row "01.tif" is still listed and the CHECKPOINTS list shows 1 row.
28. Click the second frame row (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif"; row 1 shows the eye icon and the red trash icon.
29. Click **Remove**, the red trash icon (`FFImageRow::list-remove-button` 0) on the second row.
   Expect: the dialog titled "Remove and fuse again?" is shown with the buttons **Cancel**, **Exclude instead** and **Remove and re-fuse**.
30. Click **Remove and re-fuse** (`AppWindow::frame-warning`).
   Expect: the "Remove and fuse again?" dialog is gone and the "Keep this fused result?" dialog is shown (within 60 s).
31. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone; the CHECKPOINTS list shows 2 rows; the dock list no longer has "01.tif" and its second row reads "02.tif" (within 60 s).
