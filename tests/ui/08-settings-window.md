# Settings window: performance, storage, input map, diagnostics and about
Start: --open fixtures/mantis-64/frames
## Open the window
1. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: the "Settings" window opens (`AppWindow::settings-modal`) with six rail entries Performance, Appearance, Storage, Input map, Diagnostics and About; the content shows "GPU processing" with its switch knob at the right end on an accent-coloured track (within 1 s).
2. Click **Fuse stack** (`FFToolbar::fuse-button`) while the Settings window is open.
   Expect: Must not: a fuse starts; **Fuse stack** is still enabled (read with `buttons AppWindow::toolbar`) and the Settings window is still open.
## Performance
3. Click **Performance** (`FFNavRailEntry::area` 0).
   Expect: the content shows the headings "PROCESSING" and "LIMITS" and the labels "GPU processing", "Backend", "Host memory" and "VRAM", and no label "Host budget" or "VRAM budget"; the Host memory row shows a whole number beside a unit field reading "GiB" or "MiB"; the VRAM row shows the grey hint "Not probed yet" in its number field beside a unit field reading "GiB"; the "Backend" row shows no value text (within 1 s).
4. Click the GPU processing switch (`FFToggle::area` 1).
   Expect: the switch knob moves to the left end and its track turns grey; the "Backend" row still shows no value text.
5. Click the GPU processing switch (`FFToggle::area` 1).
   Expect: the switch knob is back at the right end on an accent-coloured track.
6. Click the GPU processing switch (`FFToggle::area` 1).
   Expect: the knob is at the left end on a grey track again.
7. Type "99999999" into the Host memory number (`FFTextField::input` 0).
   Expect: the field shows a longer whole number containing "99999999" in the neutral tone, and no red line shows under the Host memory row (a typed number counts only once it is confirmed).
8. Press Return.
   Expect: a red line reading "Max", a size ending in "GiB" or "MiB", and "possible." shows under the Host memory row; the number field shows a short whole number again, not the long digits.
9. Click the VRAM unit field reading "GiB" (`FFMenu::root`, match 1; match 0 is "Host memory").
   Expect: a list with the lines "MiB" and "GiB" opens under the unit field (within 1 s).
10. Click "MiB" (`FFMenuList::option-area` 0).
    Expect: the list is gone, the VRAM unit field reads "MiB" and the VRAM number field is still empty with the grey hint "Not probed yet".
11. Type "512" into the VRAM number (`FFTextField::input` 1).
    Expect: the field shows "512" in the neutral tone beside "MiB" and the grey hint is gone; Must not: the note "Caps only lower the probed budgets. Changes apply from the next start." appears below the LIMITS rows (it shows only after a run has probed the GPU).
12. Click **Close settings** (`FFSettingsModal::close`).
   Expect: the "Settings" window is gone; the canvas panes are visible again (within 1 s).
13. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: the "Settings" window is open again (within 1 s).
14. Click **Performance** (`FFNavRailEntry::area` 0).
    Expect: "GPU processing" shows with its switch knob still at the left end on a grey track, the Host memory row shows its default number again, and the VRAM row reads "512" beside "MiB" (closing the window confirmed the typed number) (within 1 s).
## Storage
15. Click **Storage** (`FFNavRailEntry::area` 2).
    Expect: the heading "LOCATIONS" shows the rows "Settings file", "Logs", "Crash reports" and "Scratch", each with a path text (long paths are cut off with "…"; a folder not yet created adds the grey text "not created yet") and none with the grey text "Unavailable"; "GPU processing" is gone (within 1 s).
16. Click the "Stage cache" row label (`FFSettingsActionRow::label`); **Clear cache** is only looked at, never pressed.
    Expect: Must not: the line under the row reads "Cleared"; the heading "CACHE" shows the row "Stage cache" with the grey text "Deletes cached fuse stages." and an enabled **Clear cache** button (read with `buttons AppWindow::settings-modal`), and the line under the row is empty.
## Input map
17. Click **Input map** (`FFNavRailEntry::area` 3).
    Expect: the heading "BINDINGS" shows a line under it that names either the built-in bindings ("Built-in bindings.") or the settings file ("Read from <path to settings.toml>: <n> bindings differ from the built-in ones."; which one depends on whether this machine already has a settings file) and no red line; below it a table header "INPUT", "STACKING", "EDITOR"; the INPUT column (`FFInputTableRow::input-cell`) holds six rows, top to bottom "wheel", "ctrl+wheel", "shift+wheel", "middle-drag / space+left-drag", "ctrl+z / ctrl+shift+z" and "alt+left-click" (within 1 s).
18. Click the header "STACKING" (`FFInputTableRow::stacking-cell` 0).
    Expect: the STACKING column holds six rows, top to bottom "zoom-at-cursor", "step-source-frame", "pan-horizontal", "pan", "—" and "pick-source-under-pointer".
19. Click the header "EDITOR" (`FFInputTableRow::editor-cell` 0).
    Expect: the EDITOR column holds six rows, top to bottom "zoom-at-cursor", "step-source-frame", "brush-size", "pan", "undo / redo" and "pick-source-under-pointer".
20. Click the text "wheel" in the first row (`FFInputTableRow::input-cell` 1).
    Expect: Must not: a field opens or a row changes; the table still shows six rows.
## Diagnostics
21. Click **Diagnostics** (`FFNavRailEntry::area` 4).
    Expect: (within 10 s) the heading "DIAGNOSTICS" shows a file list holding the rows "adapter.json", "configuration.json", "system.json" and "version.json", each followed by a size, and no "Collecting…" text; "BINDINGS" is gone.
22. Click the Include minidumps switch (`FFToggle::area` 1).
    Expect: the switch knob is at the right end on an accent-coloured track and the amber text "Not redacted — binary memory contents. Marked in the list above." appears under it.
23. Click the Include minidumps switch (`FFToggle::area` 1).
    Expect: the knob is at the left end on a grey track and the "Not redacted" text is gone.
24. Click the "Save to" field (`FFDiagnosticsPanel::dest-field`); **Create bundle** is only looked at, never pressed.
    Expect: the field shows a non-empty path text (the field is narrower than the path, so its end is cut off); the result line (`FFDiagnosticsPanel::message`) is empty and **Create bundle** is listed enabled (read with `buttons AppWindow::settings-modal`).
## About
25. Click **About** (`FFNavRailEntry::area` 5).
    Expect: the heading "FOCUSFORGE" shows "Version" with a number of the form "0.1.0" and "License" with "GPL-3.0"; the heading "NOTICE" shows the line "Copyright © 2026 Johannes Sood" and a paragraph starting "FocusForge is free software"; the row "Source" shows the accent-coloured text "github.com/jjjsood/FocusForge" (within 1 s).
## The backend line after a fuse with the GPU switch off
26. Press Escape (step 24 focused the "Save to" field and step 25 switched to About).
    Expect: the "Settings" window and its rail are gone; the canvas panes are visible again (within 1 s).
27. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep".
28. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bottom bar status reads "fused" followed by a time (for example "fused 16:34") (within 1 s).
29. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: the "Settings" window is open (within 1 s).
30. Click **Performance** (`FFNavRailEntry::area` 0).
    Expect: the "Backend" row reads "cpu — GPU processing is off" and the GPU switch knob is at the left end on a grey track (within 1 s).
31. Click the GPU processing switch (`FFToggle::area` 1).
    Expect: the knob is at the right end on an accent-coloured track.
32. Click **Close settings** (`FFSettingsModal::close`).
    Expect: the "Settings" window is gone (within 1 s).
