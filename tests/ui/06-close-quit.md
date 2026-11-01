# Close a tab and quit the window with unsaved work
Start: --open fixtures/mantis-64/frames --open fixtures/mantis-512/frames --fuse-tabs 0,1 --request-close
## The close question for the active tab
1. Click **Keep** (`AppWindow::divergence-warning`) as soon as the dialog "Keep this fused result?" shows the line "00.tif and 01.tif could not be aligned and were fused as they are." (both fuses run after launch; the 8-frame tab is the active one).
   Expect: (within 90 s) the dialog "Keep this fused result?" is replaced by a dialog titled "Save this state first?" with the line "Closing frames", the text "1 unsaved checkpoint." and the note "Saving asks where to write the project file."; its buttons read "Cancel", "Save and close" and "Close anyway", beside a **Close dialog** icon button at its top-right corner.
2. Click **Cancel** (`AppWindow::close-guard`).
   Expect: the dialog is gone; the tab strip shows two tabs titled "frames", each carrying an "UNSAVED" marker; the dock header reads "8 frames".
## The other tab waits on its own question
3. Click the first tab (`FFTab::area` 0).
   Expect: the dialog "Keep this fused result?" is shown with the line "00.tif and 02.tif could not be aligned and were fused as they are."
4. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone, the CHECKPOINTS list shows 1 row and the dock header reads "3 frames".
5. Click the close glyph of the first tab (`FFTab::close-area` 0).
   Expect: a dialog titled "Save this state first?" appears with the line "Closing frames" and the text "1 unsaved checkpoint.".
## Cancel keeps both tabs
6. Click **Cancel** (`AppWindow::close-guard`).
   Expect: the dialog is gone; the tab strip shows two tabs, each carrying an "UNSAVED" marker.
7. Click **Close** (`FFTabStrip::window-close`).
   Expect: a dialog titled "Quit without saving?" appears with the line "Quitting FocusForge" and a text starting "2 projects have unsaved" and ending "Quitting now loses all of it."; its buttons read "Cancel", "Save all and quit" and "Quit without saving", beside a **Close dialog** icon button at its top-right corner.
8. Click **STACKING** (`FFModeSwitch::stacking-half`) while the quit question is open.
   Expect: Must not: the toolbar row shows a **Fuse stack** button; the dialog "Quit without saving?" is still shown.
9. Click **Cancel** (`AppWindow::close-guard`).
   Expect: the dialog is gone; the window is still open and the tab strip shows two tabs, each carrying an "UNSAVED" marker.
10. Click **Close** (`FFTabStrip::window-close`).
   Expect: a dialog titled "Quit without saving?" appears again with the line "Quitting FocusForge".
11. Press Escape while the quit question is open.
   Expect: Must not: the window closes; the dialog is gone, the window is still open and the tab strip shows two tabs, each carrying an "UNSAVED" marker.
12. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the toolbar row shows a **Fuse stack** button.
## Close one tab, keep the other
13. Click the close glyph of the second tab (`FFTab::close-area` 1).
   Expect: a dialog titled "Save this state first?" appears with the line "Closing frames", not "Quitting FocusForge".
14. (human only) Click **Save and close** (`AppWindow::close-guard`), then cancel the file chooser that opens.
   Expect: the file chooser is gone; the tab strip still shows two tabs and no "No project open" text is shown.
15. Click the close glyph of the second tab (`FFTab::close-area` 1).
   Expect: the dialog titled "Save this state first?" is shown with the line "Closing frames".
16. Click **Close dialog** (`AppWindow::close-guard`), the icon button at the dialog's top-right corner.
   Expect: Must not: the tab closes; the dialog is gone and the tab strip still shows two tabs, each carrying an "UNSAVED" marker.
17. Click the close glyph of the second tab (`FFTab::close-area` 1).
   Expect: the dialog titled "Save this state first?" is shown with the line "Closing frames".
18. Click **Close anyway** (`AppWindow::close-guard`).
   Expect: the dialog is gone; the tab strip shows one tab titled "frames" carrying an "UNSAVED" marker; the text "No project open" is not shown.
19. Click **STACKING** (`FFModeSwitch::stacking-half`).
   Expect: the dock header reads "3 frames · 0 excluded", so the remaining tab is the first one.
## The quit question with one unsaved project
20. Click **Close** (`FFTabStrip::window-close`).
   Expect: a dialog titled "Quit without saving?" appears with a text starting "1 project has unsaved" and ending "Quitting now loses all of it."
21. Click **Cancel** (`AppWindow::close-guard`).
   Expect: the dialog is gone; the window is still open and the tab strip still shows the tab "frames" with its "UNSAVED" marker.
22. Click **Close** (`FFTabStrip::window-close`).
   Expect: the dialog titled "Quit without saving?" appears again with the line "Quitting FocusForge".
23. Click **Close dialog** (`AppWindow::close-guard`), the icon button at the dialog's top-right corner.
   Expect: Must not: the window closes; the dialog is gone, the window is still open and the tab strip still shows the tab "frames" with its "UNSAVED" marker.
24. Click **Close** (`FFTabStrip::window-close`).
   Expect: the dialog titled "Quit without saving?" appears again with the line "Quitting FocusForge".
25. (human only) Click **Save all and quit** (`AppWindow::close-guard`), then cancel the file chooser that opens.
   Expect: the file chooser is gone; the window is still open and the tab "frames" is still open with its "UNSAVED" marker.
26. Click **Close** (`FFTabStrip::window-close`).
   Expect: the dialog titled "Quit without saving?" is shown with a text starting "1 project has unsaved" and ending "Quitting now loses all of it."
## Quit without saving
27. Click **Quit without saving** (`AppWindow::close-guard`).
   Expect: the window closes, so the next screenshot attempt finds no window.
28. (human only) Look at the desktop after the previous step.
   Expect: no FocusForge window remains and the FocusForge process has ended.
