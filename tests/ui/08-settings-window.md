# Settings window: performance, storage, input map, diagnostics and about
Start: --open fixtures/mantis-64/frames
## Open the window
1. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
   Expect: the "Settings" window opens (`AppWindow::settings-modal`) with six rail entries Performance, Appearance, Storage, Input map, Diagnostics and About; the content shows "GPU processing" with its switch knob at the right end on an accent-coloured track.
2. Click **Fuse stack** (`FFToolbar::fuse-button`) while the Settings window is open.
   Expect: Must not: a fuse starts; **Fuse stack** is still enabled (read with `buttons AppWindow::toolbar`) and the Settings window is still open.
## Performance
3. Click **Performance** (`FFNavRailEntry::area` 0).
   Expect: the content shows the headings "PROCESSING" and "LIMITS" and the labels "GPU processing", "Backend", "Host memory cap", "VRAM cap", "Host budget" and "VRAM budget"; the two cap fields are empty and show the grey hint "No cap"; "Host budget" reads a size ending in "GiB" or "MiB"; "VRAM budget" reads "Not probed yet"; the "Backend" row shows no value text.
4. Click the GPU processing switch (`FFToggle::area` 1).
   Expect: the switch knob moves to the left end and its track turns grey; the "Backend" row still shows no value text.
5. Click the GPU processing switch (`FFToggle::area` 1).
   Expect: the switch knob is back at the right end on an accent-coloured track.
6. Click the GPU processing switch (`FFToggle::area` 1).
   Expect: the knob is at the left end on a grey track again.
7. Type "xyz" into the "Host memory cap" field (`FFTextField::input` 0).
   Expect: the field shows "xyz" with a red outline (the failure tone) and the "Host budget" value is unchanged.
8. Type "1GiB" into the "VRAM cap" field (`FFTextField::input` 1).
   Expect: the text "1GiB" shows in the neutral tone (no red border) and "VRAM budget" still reads "Not probed yet"; Must not: the note "Caps only lower the probed budgets. Changes apply from the next start." appears below the LIMITS rows (it shows only after a run has probed the GPU).
## The GPU switch persists across closing and reopening
9. Click **Close settings** (`FFSettingsModal::close`).
   Expect: the "Settings" window is gone; the canvas panes are visible again.
10. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: the "Settings" window is open again.
11. Click **Performance** (`FFNavRailEntry::area` 0).
    Expect: "GPU processing" shows with its switch knob still at the left end on a grey track.
## Storage
12. Click **Storage** (`FFNavRailEntry::area` 2).
    Expect: the heading "LOCATIONS" shows the rows "Settings file", "Logs", "Crash reports" and "Scratch", each with a path text (long paths are cut off with "…"; a folder not yet created adds the grey text "not created yet") and none with the grey text "Unavailable"; "GPU processing" is gone.
13. Click the "Stage cache" row label (`FFSettingsActionRow::label`); **Clear cache** is only looked at, never pressed.
    Expect: Must not: the line under the row reads "Cleared"; the heading "CACHE" shows the row "Stage cache" with the grey text "Deletes cached fuse stages." and an enabled **Clear cache** button (read with `buttons AppWindow::settings-modal`), and the line under the row is empty.
## Input map
14. Click **Input map** (`FFNavRailEntry::area` 3).
    Expect: the heading "BINDINGS" shows a table header "INPUT", "STACKING", "EDITOR"; the INPUT column (`FFInputTableRow::input-cell`) holds six rows, top to bottom "wheel", "ctrl+wheel", "shift+wheel", "middle-drag / space+left-drag", "ctrl+z / ctrl+shift+z" and "alt+left-click".
15. Click the header "STACKING" (`FFInputTableRow::stacking-cell` 0).
    Expect: the STACKING column holds six rows, top to bottom "zoom-at-cursor", "step-source-frame", "pan-horizontal", "pan", "—" and "pick-source-under-pointer".
16. Click the header "EDITOR" (`FFInputTableRow::editor-cell` 0).
    Expect: the EDITOR column holds six rows, top to bottom "zoom-at-cursor", "step-source-frame", "brush-size", "pan", "undo / redo" and "pick-source-under-pointer".
17. Click the text "wheel" in the first row (`FFInputTableRow::input-cell` 1).
    Expect: Must not: a field opens or a row changes; the table still shows six rows.
## Diagnostics
18. Click **Diagnostics** (`FFNavRailEntry::area` 4).
    Expect: (within 10 s) the heading "DIAGNOSTICS" shows a file list holding the rows "adapter.json", "configuration.json", "system.json" and "version.json", each followed by a size, and no "Collecting…" text; "BINDINGS" is gone.
19. Click the Include minidumps switch (`FFToggle::area` 1).
    Expect: the switch knob is at the right end on an accent-coloured track and the amber text "Not redacted — binary memory contents. Marked in the list above." appears under it.
20. Click the Include minidumps switch (`FFToggle::area` 1).
    Expect: the knob is at the left end on a grey track and the "Not redacted" text is gone.
21. Click the "Save to" field (`FFDiagnosticsPanel::dest-field`); **Create bundle** is only looked at, never pressed.
    Expect: the field shows a non-empty path text (the field is narrower than the path, so its end is cut off); the result line (`FFDiagnosticsPanel::message`) is empty and **Create bundle** is listed enabled (read with `buttons AppWindow::settings-modal`).
## About
22. Click **About** (`FFNavRailEntry::area` 5).
    Expect: the heading "FOCUSFORGE" shows "Version" with a number of the form "0.1.0" and "License" with "GPL-3.0"; the heading "NOTICE" shows the line "Copyright © 2026 Johannes Sood" and a paragraph starting "FocusForge is free software"; the row "Source" shows the accent-coloured text "github.com/jjjsood/FocusForge".
## The backend line after a fuse with the GPU switch off
23. Press Escape (step 21 focused the "Save to" field and step 22 switched to About).
    Expect: the "Settings" window and its rail are gone; the canvas panes are visible again.
24. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep".
25. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone and the bottom bar status reads "fused" followed by a time (for example "fused 16:34").
26. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: the "Settings" window is open.
27. Click **Performance** (`FFNavRailEntry::area` 0).
    Expect: the "Backend" row reads "cpu — GPU processing is off" and the GPU switch knob is at the left end on a grey track.
28. Click the GPU processing switch (`FFToggle::area` 1).
    Expect: the knob is at the right end on an accent-coloured track.
29. Click **Close settings** (`FFSettingsModal::close`).
    Expect: the "Settings" window is gone.
