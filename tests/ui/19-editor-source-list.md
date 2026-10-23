# Editor source list in Image list, Large previews and List
Start: --open fixtures/mantis-512/frames

Use a fresh settings directory, where the source presentation is Image list. The fixture fuse ends in the keep question; the journey answers **Keep** and the window then shows the editor. The Settings window opens on Performance when it is reopened from the icon bar (bottom-left): click **Appearance** each time. Source rows are counted in visible order: when the list scrolls, instance 0 is the first row on screen.

## Image list before fusing
1. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: the Settings window opens.
2. Click **Appearance** (`FFNavRailEntry::area` 1).
   Expect: the field Presentation reads "Image list" and the text "Editor source rows stay read-only." is shown.
3. Click **Close settings** (`FFSettingsModal::close`).
   Expect: (within 10 s) the Images dock lists 00.tif to 03.tif (the rows that fit), each with a preview picture at its left, a colour bar above its name, and no row reads "Loading…" or "Preview unavailable".
4. Click the second source row (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif"; the row shows an eye and a red trash icon at its lower right (`buttons FFDock::images-panel` lists **Exclude** `FFImageRow::image-list-exclude-button` and **Remove** `FFImageRow::image-list-remove-button`).
## Fuse and keep
5. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 120 s) the "Keep this fused result?" dialog is shown.
6. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone; the CHECKPOINTS list shows 1 row; the bottom bar reads "fused" followed by a time (within 60 s).
## Editor rows follow Image list and stay read-only
7. Click **EDITOR** (`FFModeSwitch::editor-half`).
   Expect: the dock header reads "8 frames" with no Images or Settings tab; each visible row shows a preview picture at its left, a colour bar above its name, and its source number ("1", "2", "3") at its lower right; no row reads "Loading…".
8. Hover the second source row (`FFImageRow::area` 1).
   Expect: Must not: an eye or trash icon appears; `buttons FFDock::images-panel` lists no **Exclude** and no **Remove**; the row still shows its number "2" at the lower right.
9. Click the second source row (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif"; the row has an orange outline all around, and still shows its number "2", with no eye or trash icon.
10. Click the first source row (`FFImageRow::area` 0).
   Expect: the left pane label reads "00.tif"; the row has an orange outline and shows its number "1".
11. Drag the first source row (`FFImageRow::area` 0) to the centre of the third source row (`FFImageRow::area` 2), using the logical element geometry without scale conversion.
   Expect: Must not: the rows reorder or the selection moves; the first three rows still read 00.tif, 01.tif, 02.tif from top to bottom and the left pane label still reads "00.tif".
## Large previews in the editor
12. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: the Settings window opens.
13. Click **Appearance** (`FFNavRailEntry::area` 1).
   Expect: the field Presentation reads "Image list" and the text "Editor source rows stay read-only." is shown.
14. Click **Presentation** (`FFMenu::root`).
   Expect: the three lines "List", "Image list" and "Large previews" appear.
15. Click the entry **Large previews**, the last of the three (`FFMenuList::option-area` 2).
   Expect: the list is gone and the Presentation field reads "Large previews".
16. Click **Close settings** (`FFSettingsModal::close`).
   Expect: the first dock row is a wide preview picture (`FFImageRow::large-identity`) with a colour square, "00.tif" and its number "1" on the line below it; the second row's preview picture starts under it; neither reads "Loading preview…".
17. Hover the number of the first source row (`FFImageRow::large-timestamp-slot` 0).
   Expect: Must not: an eye or trash icon replaces the number; `buttons FFDock::images-panel` lists no **Exclude** and no **Remove**, and the number "1" is still shown.
18. Click the preview picture of the second source row (`FFImageRow::large-identity` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif"; the second row has an orange outline.
## List in the editor
19. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: the Settings window opens.
20. Click **Appearance** (`FFNavRailEntry::area` 1).
   Expect: the field Presentation reads "Large previews".
21. Click **Presentation** (`FFMenu::root`).
   Expect: the three lines "List", "Image list" and "Large previews" appear.
22. Click the entry **List**, the first of the three (`FFMenuList::option-area` 0).
   Expect: the list is gone and the Presentation field reads "List".
23. Click **Close settings** (`FFSettingsModal::close`).
   Expect: the dock lists 00.tif to 07.tif, one compact line each with a colour square, the name and the numbers "1" to "8" at the right end; no preview pictures.
24. Click the fifth source row (`FFImageRow::area` 4).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "04.tif"; the row shows number "5" and no eye or trash icon.
## Stacking mode keeps its controls
25. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the toolbar shows **Fuse stack**; the dock shows the Images and Settings tabs and the header "8 frames · 0 excluded" with the add and sort icons at its right end.
26. Click the first source row (`FFImageRow::area` 0).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "00.tif"; an eye and a red trash icon replace the capture time on that row (`buttons FFDock::images-panel` lists **Exclude** `FFImageRow::list-exclude-button` and **Remove** `FFImageRow::list-remove-button`).
