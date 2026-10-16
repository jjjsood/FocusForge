# Images panel
Reach: open a folder (`--open`); the **Images** tab (`FFDockTabs::images-tab`) of the right dock lists the frames. Needs the STACKING mode and no run in progress to change anything. After a fuse the window switches to the editor; click **STACKING** (`FFModeSwitch::stacking-half`) to edit the list again.
Sub-features: header text ("8 frames · 0 excluded"; "1 frame" in the singular; the editor shows "8 frames"), select a row, exclude and include (eye), remove (trash), sort menu (**Name** / **Capture time**, ✓ on the chosen one, cleared by a manual drag), drag-reorder by the grip, add frames (folder-plus icon), read-only legend in the editor.
Ids: `FFImageRow::area` N (row N), `FFImageRow::exclude-area`, `FFImageRow::remove-area`, `FFImagesPanel::add-button`, `FFImagesPanel::sort-button`, `FFImagesPanel::grip-overlay`, `FFMenuList::option-area` 0 (Name) and 1 (Capture time), dialog `AppWindow::frame-warning`.
Gotchas:
- The eye and trash icons exist only on the row under the pointer, so each has exactly one match, number 0; clicking a row leaves the pointer on it, which is what brings them up. After a dialog button is clicked the pointer is elsewhere: click the row again first.
- Sort and add icons are absent, not greyed, in the editor and while a run is live; the panel dims while a run is live.
- Removing a frame before any checkpoint happens at once with no dialog. With a checkpoint, remove, sort, drag and add raise `AppWindow::frame-warning` ("Remove and fuse again?", buttons Cancel, "Exclude instead", "Remove and re-fuse"); Cancel changes nothing; confirming applies the change, re-fuses and ends in `AppWindow::divergence-warning`, then appends a second checkpoint (the first stays). Exclude is never gated.
- A default fuse of `fixtures/mantis-512` ends in the "Keep this fused result?" dialog; after **Keep** the window is in the editor.
- A manual drag clears the sort choice; picking **Name** again restores the file-name order. The sort menu closes when the sort button is pressed again.
- `--frame-remove-warning N` removes frame N right after open, before any checkpoint, so it removes at once with no warning; it only warns for a saved project. `--images-sort-menu-open` starts with the sort menu open.
- A scripted drag of the grip is not possible: it presses at the centre of `FFImagesPanel::grip-overlay`, which lies below the last row of an 8-frame list. A step after the drag must not depend on it having happened (the sort check mark stays on "Name" without a drag and is cleared with one).
- Human only: dragging the grip, the native file dialog behind the add icon, wheel scrolling the list, judging the frame thumbnails.
Guides: 10-images-panel.md
