# Source image presentations
Start: --open fixtures/mantis-512/frames --settings-section appearance --dock-tab images

Use a fresh settings directory. Presentation choices belong to the open Appearance modal. Reopen Settings with the Settings icon in the icon bar (bottom-left); Appearance remains selected. Source instances are numbered in visible order. For each drag, resolve the second visible preview image (`FFImageRow::large-identity`, instance 1) immediately before starting, and drop at its centre. The filename or timestamp below that preview is not the drop target. Use the logical coordinates returned by `get_element_properties` for every drag target: centre = absolutePosition + size / 2. Never multiply these coordinates by the window scale factor or derive them from screenshot pixels. Keep the pointer inside the source viewport. A normal filename click also places the pointer over that source before locating its eye or delete button.

## Appearance and List
1. Click **Presentation** (`FFMenu::root`).
   Expect: List, Image list and Large previews appear; Image list is checked.
2. Click **Presentation** (`FFMenu::root`) again.
   Expect: Must not: the presentation changes; the list closes and Presentation still reads Image list.
3. Click **Presentation** (`FFMenu::root`).
   Expect: List, Image list and Large previews appear; Image list is checked.
4. Click **List** (`FFMenuList::option-area` 0).
   Expect: the menu closes and Presentation reads List.
5. Click **Close settings** (`FFSettingsModal::close`).
   Expect: the modal disappears; source rows are 26 px high.
6. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: Settings opens on Appearance with List selected.
7. Click **Presentation** (`FFMenu::root`).
   Expect: the three presentation choices appear.
8. Click **Large previews** (`FFMenuList::option-area` 2).
   Expect: the menu closes and Presentation reads Large previews.
9. Click **Close settings** (`FFSettingsModal::close`).
   Expect: source rows are 200 px high with a 224×149 aspect-fit image.

## Large previews and source actions
10. Click **first visible source filename** (`FFImageRow::large-name-slot`).
   Expect: that source receives a selection marker.
11. Drag **first visible source filename** (`FFImageRow::large-name-slot`) to the centre of the second visible preview image (`FFImageRow::large-identity`, instance 1), using its logical element geometry without scale conversion.
   Expect: the first two source names exchange positions; no drag handle appears.
12. Drag **first visible source image** (`FFImageRow::large-identity`) to the centre of the second visible preview image (`FFImageRow::large-identity`, instance 1), using its logical element geometry without scale conversion.
   Expect: the first two source names return to their order before the filename drag.
13. Hover the timestamp of the first Large preview row (`FFImageRow::large-timestamp-slot` 0).
   Expect: eye and delete icons replace the timestamp of that row (`buttons FFDock::images-panel` lists **Exclude** `FFImageRow::large-exclude-button` and **Remove** `FFImageRow::large-remove-button`); the filename beside them reads "00.tif" and the second row still shows "—" as its timestamp.
14. Click **first visible source filename** (`FFImageRow::large-name-slot`).
   Expect: the first source is selected and eye/delete are visible in its timestamp area.
15. Click **Exclude**, the eye on the first source (`FFImageRow::large-exclude-button`).
   Expect: that source dims without changing the source order or count.
16. Click **second visible source filename** (`FFImageRow::large-name-slot`) (instance 1).
   Expect: the second source is selected with eye/delete visible; the excluded first source stays dimmed.
17. Click **Remove**, the single visible delete button on the hovered second source (`FFImageRow::large-remove-button`, instance 0). Only the hovered row mounts its action buttons; do not use source-row instance 1 for this button.
   Expect: source count decreases by one; no confirmation appears before a first checkpoint.

## Back to Image list
18. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: Settings opens on Appearance with Large previews selected.
19. Click **Presentation** (`FFMenu::root`).
   Expect: the three presentation choices appear.
20. Click **Image list** (`FFMenuList::option-area` 1).
   Expect: the menu closes and Presentation reads Image list.
21. Click **Close settings** (`FFSettingsModal::close`).
   Expect: rows return to 120 px; remaining source order and exclusion state persist.
