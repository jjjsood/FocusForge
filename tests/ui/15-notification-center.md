# Notifications: bell, center, toast and Projects
Start: --open fixtures/mantis-512/frames --dock-tab settings

Use a fresh settings directory and a fresh data directory. The 48 px icon bar at the left edge holds the bell (`FFIconBar::bell`, with a number badge when records are unread) above Settings (`FFIconBar::settings`). The bell opens and closes the notification center, a 380 px panel that slides in from the icon bar. Records are listed newest first; each row shows a kind icon, a title and a context line such as `frames · 03.tif · 13:13` (project title, frame file, time). The project title is the folder name, `frames`. Clicking a row selects it and shows its full message.

A toast shows only while an app window holds keyboard focus; a record that arrives while the window is in the background becomes a desktop banner and no toast shows. Every step that expects or clicks a toast therefore starts `(human only)`: run those in a window you have clicked into. The other steps read the same state through the bell: opening the center reads every record, so the bell number counts only the records that arrived since the center was last opened.

Equalize brightness is on by default. With the Target brightness (stops) slider at +0.5 and Max brightness gain at 1.25, every frame needs a gain of about 1.41, which the bound limits, so one run of `mantis-512` publishes 8 records titled "Brightness target limited", one per frame (00.tif to 07.tif). Keeping the unaligned result adds a ninth record, "4 frames unaligned". A repeated run reuses cached results and ends within about 15 s; the first run takes up to 60 s. Records stay for the session. In a focused window the toast queue shows one toast at a time, each for 6 seconds, oldest first, so the queue of 9 lasts about a minute; the toast shows "Open notification" and an X, at the top left of the canvas under the toolbar. Opening the center reads every record and clears the queue. Reopening the center keeps the row selected last.

With only PREPARE's More... open, the sliders in the dock are `FFSlider::drag-area` instance 0 (Max brightness gain) and instance 1 (Target brightness). For a drag on a slider, read its track with `get_element_properties` immediately before: the left end is absolutePosition.x, the right end is absolutePosition.x + width, and the pointer height is absolutePosition.y + 10. Use these logical coordinates as they are, never multiplied by the window scale. A drop 40 px right of the right end sets the slider to its maximum, +0.5. The target readout is signed: zero is `0.0`, a positive value carries a plus. A row card is `FFNotificationRow::card`; its instances count the rows in the list from the top of the visible part of the list. A key step (`Press`) goes to the control holding the keyboard focus, which the click of the step before gave it. The crash steps need a data folder that already holds a crash report and start `(human only)`.

## Empty center
1. Click **Notifications** (`FFIconBar::bell`).
   Expect: the center opens to the right of the icon bar reading "Notifications", "0 notifications · this session", "No notifications yet" and "Messages will remain here for this session."; the bell shows no number.
2. Click **Notifications** (`FFIconBar::bell`).
   Expect: the center is gone and **Fuse stack** is visible in the toolbar again.

## Raise the brightness warnings
3. Click **More...** under PREPARE (`FFMoreToggle::header-area`, instance 0).
   Expect: the PREPARE section lists Equalize brightness, Max brightness gain reading 1.25 and Target brightness (stops) reading 0.0.
4. Drag **Target brightness track** (`FFSlider::drag-area`, instance 1) to 40 px right of its right end.
   Expect: the target readout reads +0.5 and the max gain readout still reads 1.25.
5. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: the dialog "Keep this fused result?" appears (within 90 s) and the bell shows the number 8.
6. Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone; the bell shows the number 9.

## Open the center from the bell
7. Click **Notifications** (`FFIconBar::bell`).
   Expect: the center opens reading "9 notifications · this session"; the bell number is gone; the first listed row is titled "4 frames unaligned" and no row has an orange outline.
8. Click the first listed row (`FFNotificationRow::card`, instance 0).
   Expect: that row has an orange outline and shows the message "Kept the fused result with 4 frames unaligned."; the rows below it list `07.tif` above `06.tif` above `05.tif`.
9. Click the second listed row (`FFNotificationRow::card`, instance 1).
   Expect: the second row has an orange outline and shows its full message; the first row is collapsed and shows no message.

## Keyboard in the center
10. Press Down.
    Expect: the orange outline moves to the next row down and that row shows no message.
11. Press Return.
    Expect: the selected row shows its full message.
12. Press Escape.
    Expect: the center closes, the Projects overview does not open, and the bell has an orange outline.
13. Press Return.
    Expect: the center opens again.

## Close, then Projects hides the center
14. Click **Close notifications** (`AppWindow::notification-center`).
    Expect: the center is gone and the bell shows no number.
15. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the Projects overview opens to the right of the icon bar; the bell and Settings stay visible in the icon bar.
16. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview is gone and the center stays closed.

## A second run, counted from the bell
17. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: **Fuse stack** shows in the toolbar.
18. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: the dialog "Keep this fused result?" appears (within 60 s) and the bell shows the number 8, not 17.
19. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bell shows the number 9.
20. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview opens with the center closed; the bell shows the number 9 in the icon bar.
21. Click **Notifications** (`FFIconBar::bell`).
    Expect: the overview is gone and the center is open reading "18 notifications · this session"; the bell number is gone.
22. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: Must not: the center stays open next to the overview; the overview shows, the center is gone and the bell shows no number.

## Toasts (window focused)
23. (human only) Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overview is gone.
24. (human only) Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: **Fuse stack** shows in the toolbar.
25. (human only) Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: the dialog "Keep this fused result?" appears (within 60 s) and the bell shows the number 8.
26. (human only) Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone; the bell shows the number 9; a toast with a title and "Open notification" shows at the top left of the canvas under the toolbar.
27. (human only) Click **Dismiss notification** (`FFToast::close`).
    Expect: Must not: the center opens or the number changes; the center stays closed and the bell still shows the number 9.
28. (human only) Click the toast (`FFToast::card`) that shows next.
    Expect: the center opens reading "27 notifications · this session"; the bell number is gone and no toast shows; one row has an orange outline and shows its full message, and that row carries the title the toast showed.

## A crash record
29. (human only) Quit the window. Make a data folder holding `focusforge/crashes/20260918T010203Z-focusforge-gui-panic.json` with the content `{"format":1,"payload":"boom"}`, then start the window again with the same arguments, a throwaway `--config-dir` and `FOCUSFORGE_DATA_DIR` set to that folder.
    Expect: a toast titled "Previous session stopped" with an amber stripe and "Open notification" shows at the top left of the canvas; the bell shows the number 1.
30. (human only) Move the pointer onto the toast and keep it there for 20 seconds.
    Expect: the toast is still showing after 20 seconds, twice its 10 second lifetime.
31. (human only) Move the pointer off the toast.
    Expect: the toast is gone within 10 seconds and the bell still shows the number 1.
32. (human only) Click **Notifications** (`FFIconBar::bell`).
    Expect: the center lists one row "Previous session stopped" with the context line "Startup · 1 crash report" and a time, and shows no message and no buttons.
33. (human only) Click the row (`FFNotificationRow::card`, instance 0).
    Expect: the row shows "A previous session stopped unexpectedly. One crash report was detected at startup." and "Reading or closing this notice does not acknowledge the report." with **Acknowledge** enabled and **Diagnostics** below.
34. (human only) Click **Acknowledge** (`AppWindow::notification-center`).
    Expect: the row shows "Acknowledged" and **Acknowledge** is greyed out; the context line still reads "Startup · 1 crash report".
35. (human only) Click **Diagnostics** (`AppWindow::notification-center`).
    Expect: the Settings window opens with its Diagnostics section selected.
