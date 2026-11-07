# Toasts in a focused window and the crash record
Start: --open fixtures/mantis-512/frames --dock-tab settings

Use a fresh settings directory and a fresh data directory. A toast shows only while an app window holds keyboard focus; a record that arrives while the window is in the background becomes a desktop banner and no toast shows. Run the toast steps in a window you have clicked into. In a focused window the toast queue shows one toast at a time, each for 6 seconds, oldest first; the toast shows "Open notification" and an X, at the top left of the canvas under the toolbar. Opening the notification center (`FFIconBar::bell`) reads every record and clears the queue.

One run of `mantis-512` with the Target brightness (stops) slider at +0.5 and Max brightness gain at 1.25 publishes 8 records titled "Brightness not fully matched", one per frame; keeping the unaligned result adds a ninth, "3 frames unaligned". The bell number counts the records that arrived since the center was last opened.

With only PREPARE's More... open, the sliders in the dock are `FFSlider::drag-area` instance 0 (Max brightness gain) and instance 1 (Target brightness). For a drag on a slider, read its track with `get_element_properties` immediately before: the left end is absolutePosition.x, the right end is absolutePosition.x + width, and the pointer height is absolutePosition.y + 10. Use these logical coordinates as they are, never multiplied by the window scale. A drop 40 px right of the right end sets the slider to its maximum, +0.5. The target readout is signed: zero is `0.0`, a positive value carries a plus. A row card is `FFNotificationRow::card`; its instances count the rows in the list from the top of the visible part of the list.

The crash steps need a data folder that already holds a crash report: step 7 makes it and starts the window again on it.
## Toasts (window focused)
1. (human only) Click **More...** under PREPARE (`FFMoreToggle::header-area`, instance 0).
   Expect: the PREPARE section lists Equalize brightness, Max brightness gain reading 1.25 and Target brightness (stops) reading 0.0.
2. (human only) Drag **Target brightness track** (`FFSlider::drag-area`, instance 1) to 40 px right of its right end.
   Expect: the target readout reads +0.5 and the max gain readout still reads 1.25.
3. (human only) Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: the dialog "Keep this fused result?" appears (within 90 s) and the bell shows the number 8.
4. (human only) Click **Keep** (`AppWindow::divergence-warning`).
   Expect: the dialog is gone; the bell shows the number 9; a toast with a title and "Open notification" shows at the top left of the canvas under the toolbar (within 2 s).
5. (human only) Click **Dismiss notification** (`FFToast::close`).
   Expect: Must not: the center opens or the number changes; the center stays closed and the bell still shows the number 9.
6. (human only) Click the toast (`FFToast::card`) that shows next.
   Expect: the center opens reading "9 notifications · this session"; the bell number is gone and no toast shows; one row has an orange outline and shows its full message, and that row carries the title the toast showed.
## A crash record
7. (human only) Quit the window. Make a data folder holding `focusforge/crashes/20260918T010203Z-focusforge-gui-panic.json` with the content `{"format":1,"payload":"boom"}`, then start the window again with the same arguments, a throwaway `--config-dir` and `FOCUSFORGE_DATA_DIR` set to that folder.
   Expect: a toast titled "Previous session stopped" with an amber stripe and "Open notification" shows at the top left of the canvas; the bell shows the number 1 (within 2 s).
8. (human only) Move the pointer onto the toast and keep it there for 20 seconds.
   Expect: the toast is still showing after 20 seconds, twice its 10 second lifetime.
9. (human only) Move the pointer off the toast.
   Expect: the toast is gone within 10 seconds and the bell still shows the number 1.
10. (human only) Click **Notifications** (`FFIconBar::bell`).
    Expect: the center lists one row "Previous session stopped" with the context line "Startup · 1 crash report" and a time, and shows no message and no buttons.
11. (human only) Click the row (`FFNotificationRow::card`, instance 0).
    Expect: the row shows "A previous session stopped unexpectedly. One crash report was detected at startup." and "Reading or closing this notice does not acknowledge the report." with **Acknowledge** enabled and **Diagnostics** below.
12. (human only) Click **Acknowledge** (`AppWindow::notification-center`).
    Expect: the row shows "Acknowledged" and **Acknowledge** is greyed out; the context line still reads "Startup · 1 crash report".
13. (human only) Click **Diagnostics** (`AppWindow::notification-center`).
    Expect: the Settings window opens with its Diagnostics section selected.
