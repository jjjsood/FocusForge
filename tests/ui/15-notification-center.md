# Notifications: bell, center and Projects
Start: --open fixtures/mantis-512/frames --dock-tab settings

Use a fresh settings directory and a fresh data directory. The 48 px icon bar at the left edge holds the bell (`FFIconBar::bell`, with a number badge when records are unread) above Settings (`FFIconBar::settings`). The bell opens and closes the notification center, a 380 px panel that slides in from the icon bar. Records are listed newest first; each row shows a kind icon, a title and a context line such as `frames · 03.tif · 13:13` (project title, frame file, time). The project title is the folder name, `frames`. Clicking a row selects it and shows its full message.

The bell reads the records: opening the center reads every record, so the bell number counts only the records that arrived since the center was last opened.

Equalize brightness is off by default; the guide switches it on. With the Target brightness (stops) slider at +0.5 and Max brightness gain at 1.25, the raised target is more than the gain limit allows, so one run of `mantis-512` publishes 8 records titled "Brightness not fully matched", one per frame (00.tif to 07.tif), each reading "<file> could not be brought to the same brightness as the other frames.". Keeping the unaligned result (00.tif, 01.tif and 06.tif, which the raised target changes from the default run's two) adds a ninth record, "3 frames unaligned". A repeated run reuses cached results and ends within about 15 s; the first run takes up to 90 s. Records stay for the session. Opening the center reads every record. Reopening the center keeps the row selected last.

With only PREPARE's More... open, the sliders in the dock are `FFSlider::drag-area` instance 0 (Max brightness gain) and instance 1 (Target brightness). For a drag on a slider, read its track with `get_element_properties` immediately before: the left end is absolutePosition.x, the right end is absolutePosition.x + width, and the pointer height is absolutePosition.y + 10. Use these logical coordinates as they are, never multiplied by the window scale. A drop 40 px right of the right end sets the slider to its maximum, +0.5. The target readout is signed: zero is `0.0`, a positive value carries a plus. A row card is `FFNotificationRow::card`; its instances count the rows in the list from the top of the visible part of the list. A key step (`Press`) goes to the control holding the keyboard focus, which the click of the step before gave it.

## Empty center
1. Click **Notifications** (`FFIconBar::bell`).
   Expect: the center opens to the right of the icon bar reading "Notifications", "0 notifications · this session", "No notifications yet" and "Messages will remain here for this session."; the bell shows no number.
2. Click **Notifications** (`FFIconBar::bell`).
   Expect: the center is gone and **Fuse stack** is visible in the toolbar again.

## Raise the brightness warnings
3. Click **More...** under PREPARE (`FFMoreToggle::header-area`, instance 0).
   Expect: the PREPARE section lists Equalize brightness, Max brightness gain reading 1.25 and Target brightness (stops) reading 0.0. The switch is off and the two sliders are greyed.
4. Click **Equalize brightness switch** (`FFToggle::area`, instance 0).
   Expect: the switch is on (orange); Max brightness gain and Target brightness are no longer greyed.
5. Drag **Target brightness track** (`FFSlider::drag-area`, instance 1) to 40 px right of its right end.
   Expect: the target readout reads +0.5 and the max gain readout still reads 1.25.
6. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: the dialog "Keep this fused result?" appears (within 90 s) and the bell shows the number 8.
7. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone; the bell shows the number 9 (within 1 s).

## Open the center from the bell
8. Click **Notifications** (`FFIconBar::bell`).
   Expect: the center opens reading "9 notifications · this session"; the bell number is gone; the first listed row is titled "3 frames unaligned" and no row has an orange outline.
9. Click the first listed row (`FFNotificationRow::card`, instance 0).
   Expect: that row has an orange outline and shows the message "Kept the fused result with 3 frames unaligned."; the rows below it list `07.tif` above `06.tif` above `05.tif`.
10. Click the second listed row (`FFNotificationRow::card`, instance 1).
   Expect: the second row has an orange outline and shows its full message; the first row is collapsed and shows no message.

## Keyboard in the center
11. Press Down.
    Expect: the orange outline moves to the next row down and that row shows no message.
12. Press Return.
    Expect: the selected row shows its full message.
13. Press Escape.
    Expect: the center closes and the Projects overview does not open.
14. Click **Notifications** (`FFIconBar::bell`).
    Expect: the center opens again.

## Close, then Projects hides the center
15. Click **Close notifications** (`AppWindow::notification-center`).
    Expect: the center is gone and the bell shows no number.
16. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the Projects overview opens to the right of the icon bar; the bell and Settings stay visible in the icon bar (within 1 s).
17. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview is gone and the center stays closed (within 1 s).

## A second run, counted from the bell
18. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: **Fuse stack** shows in the toolbar.
19. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: the dialog "Keep this fused result?" appears (within 60 s) and the bell shows the number 8, not 17.
20. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bell shows the number 9 (within 1 s).
21. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview opens with the center closed; the bell shows the number 9 in the icon bar (within 1 s).
22. Click **Notifications** (`FFIconBar::bell`).
    Expect: the overview is gone and the center is open reading "18 notifications · this session"; the bell number is gone (within 1 s).
23. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: Must not: the center stays open next to the overview; the overview shows, the center is gone and the bell shows no number (within 1 s).
