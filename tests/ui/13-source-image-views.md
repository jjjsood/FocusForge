# Source image presentations
Start: --open fixtures/mantis-512/frames --settings-section appearance --dock-tab images

Use a fresh settings directory. Presentation choices belong to the open Appearance modal. Reopen Settings with the Settings icon in the icon bar (bottom-left); Appearance remains selected. Source instances are numbered in visible order. For each drag, resolve the second visible preview image (`FFImageRow::large-identity`, instance 1) immediately before starting, and drop at its centre. The filename or timestamp below that preview is not the drop target. Use the logical coordinates returned by `get_element_properties` for every drag target: centre = absolutePosition + size / 2. Never multiply these coordinates by the window scale factor or derive them from screenshot pixels. Keep the pointer inside the source viewport. A normal filename click also places the pointer over that source before locating its eye or delete button.

## Appearance and List
1. Click **Presentation** (`FFMenu::root`).
   Expect: List, Image list and Large previews appear; Image list is checked.
2. Click **List** (`FFMenuList::root`).
   Expect: the menu closes and Presentation reads List.
3. Click **Close settings** (`FFSettingsModal::close`).
   Expect: the modal disappears; source rows are 26 px high.
4. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: Settings opens on Appearance with List selected.
5. Click **Presentation** (`FFMenu::root`).
   Expect: the three presentation choices appear.
6. Click **Large previews** (`FFMenuList::root`).
    Expect: the menu closes and Presentation reads Large previews.
7. Click **Close settings** (`FFSettingsModal::close`).
    Expect: source rows are 200 px high with a 224×149 aspect-fit image.

## Large previews and source actions
8. Click **first visible source filename** (`FFImageRow::large-name-slot`).
    Expect: that source receives a selection marker.
9. Drag **first visible source filename** (`FFImageRow::large-name-slot`) to the centre of the second visible preview image (`FFImageRow::large-identity`, instance 1), using its logical element geometry without scale conversion.
    Expect: the first two source names exchange positions; no drag handle appears.
10. Drag **first visible source image** (`FFImageRow::large-identity`) to the centre of the second visible preview image (`FFImageRow::large-identity`, instance 1), using its logical element geometry without scale conversion.
    Expect: the first two source names return to their order before the filename drag.
11. Hover the timestamp of the first Large preview row (`FFImageRow::large-timestamp-slot` 0).
    Expect: eye and delete icons replace the timestamp of that row (`buttons FFDock::images-panel` lists **Exclude** `FFImageRow::large-exclude-button` and **Remove** `FFImageRow::large-remove-button`); the filename beside them reads "00.tif" and the second row still shows "—" as its timestamp.
12. Click **first visible source filename** (`FFImageRow::large-name-slot`).
    Expect: the first source is selected and eye/delete are visible in its timestamp area.
13. Click **Exclude**, the eye on the first source (`FFImageRow::large-exclude-button`).
    Expect: that source dims without changing the source order or count.
14. Click **second visible source filename** (`FFImageRow::large-name-slot`) (instance 1).
    Expect: the second source is selected with eye/delete visible; the excluded first source stays dimmed.
15. Click **Remove**, the single visible delete button on the hovered second source (`FFImageRow::large-remove-button`, instance 0). Only the hovered row mounts its action buttons; do not use source-row instance 1 for this button.
    Expect: source count decreases by one; no confirmation appears before a first checkpoint.

## Remaining directed presentation transitions
16. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: Settings opens on Appearance with Large previews selected.
17. Click **Presentation** (`FFMenu::root`).
    Expect: the three presentation choices appear.
18. Click **Image list** (`FFMenuList::root`).
    Expect: the menu closes and Presentation reads Image list.
19. Click **Close settings** (`FFSettingsModal::close`).
    Expect: rows return to 120 px; remaining source order and exclusion state persist.
20. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: Settings opens on Appearance with Image list selected.
21. Click **Presentation** (`FFMenu::root`).
    Expect: the three presentation choices appear.
22. Click **Large previews** (`FFMenuList::root`).
    Expect: the menu closes and Presentation reads Large previews.
23. Click **Close settings** (`FFSettingsModal::close`).
    Expect: rows return to 200 px; remaining source order and exclusion state persist.
24. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: Settings opens on Appearance with Large previews selected.
25. Click **Presentation** (`FFMenu::root`).
    Expect: the three presentation choices appear.
26. Click **List** (`FFMenuList::root`).
    Expect: the menu closes and Presentation reads List.
27. Click **Close settings** (`FFSettingsModal::close`).
    Expect: rows return to 26 px; remaining source order and exclusion state persist.
28. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: Settings opens on Appearance with List selected.
29. Click **Presentation** (`FFMenu::root`).
    Expect: the three presentation choices appear.
30. Click **Image list** (`FFMenuList::root`).
    Expect: the menu closes and Presentation reads Image list.
31. Click **Close settings** (`FFSettingsModal::close`).
    Expect: rows return to 120 px; source order and exclusions persist.

## Must not work
32. (human only) Start a filename drag, observe its row following the pointer, leave the source viewport, and release.
    Expect: Must not: a drag remains active after release or a later pointer move reorders sources.

## Selection visibility
33. (human only) Use a source corpus large enough to scroll in every presentation, with at least one filename that clips in the row. In List, Image list and Large previews, wheel over a source row until selection leaves the initial viewport, then wheel back. Hover the clipped filename in each presentation.
    Expect: the canvas source changes with each wheel direction and the selected row remains visible in the source panel. The full clipped filename appears in the floating tooltip; moving to a short filename or leaving the row hides it.

## Keyboard and drag visibility
34. (human only) Use a source corpus large enough to scroll in every presentation. In List, Image list and Large previews, focus a source row, press Down until selection leaves the initial viewport, then press Up back to the first source.
    Expect: each key changes the canvas source and keeps its selected source row visible; the first and last source stay visible when their direction key is pressed at the edge.
35. (human only) In each presentation, drag a source to the top viewport edge and hold, then release it; repeat at the bottom edge. Start a press near an edge without moving, then release.
    Expect: a moved held drag auto-scrolls only within the source extent, releases with the moved source fully visible, and stops scrolling on release. A stationary edge press neither scrolls nor reorders sources.
