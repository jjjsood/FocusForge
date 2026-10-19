# Set stacking parameters, reset them and keep them per project tab
Start: --open fixtures/mantis-64/frames --open fixtures/mantis-512/frames --dock-tab settings --slow-run 1
## Locked while a fuse runs
1. Click the second project tab (`FFTab::area` 1).
   Expect: the Settings dock lists the headings "PREPARE", "FUSE", "ALIGN" and "FINISH"; the toolbar shows the **Fuse stack** button.
2. Click **Fuse stack** (`FFToolbar::fuse-button`); this is the first run of the launch, which `--slow-run 1` holds in its fuse stage for at least 150 s.
   Expect: (within 60 s) the bottom bar status reads "Aligning" or "Fusing" (the run aligns before its held fuse stage), the toolbar shows the buttons **Pause** and **Cancel**, the **Fuse stack** button is gone and the Settings panel is drawn dimmed.
3. Click **Pause** (`FFToolbar::pause-button`).
   Expect: the toolbar shows the **Resume** button instead of **Pause** and the bottom bar status reads "Paused".
4. Click the **Enabled** switch under ALIGN (`FFToggle::area` 0) while the run is paused.
   Expect: Must not: the switch turns off; the Settings panel stays dimmed and the switch stays on.
5. Click **Cancel** (`FFToolbar::cancel-button`).
   Expect: (within 30 s) the **Fuse stack** button is back in the toolbar, the buttons **Pause**, **Resume** and **Cancel** are gone and the Settings panel is no longer dimmed.
## Defaults and Reset All on the second tab
6. Drag the Settings scrollbar (`FFScrollbar::track-area`) from the top to the end of its track (`$M drag FFScrollbar::track-area 0 1193 705`).
   Expect: the row "Reset All to Default" is visible below the FINISH rows; `buttons FFDock::settings-panel` lists **Reset All to Default** with its line starting `x` (disabled); the Auto-crop switch is on and the FINISH heading shows "DEFAULT".
7. Click **Reset All to Default** (`FFResetAll::area`) while it is disabled.
   Expect: Must not: it becomes enabled or the Auto-crop switch turns off; `buttons FFDock::settings-panel` lists **Reset All to Default** still with its line starting `x`.
8. Click the **Auto-crop** switch (`FFToggle::area` 1) on the second tab.
   Expect: the switch is off, the FINISH heading has no "DEFAULT" badge and `buttons FFDock::settings-panel` lists **Reset All to Default** with its line not starting `x` (enabled).
## Values stay with their tab
9. Click the first project tab (`FFTab::area` 0).
   Expect: the Auto-crop switch is on, the FINISH heading shows "DEFAULT" and `buttons FFDock::settings-panel` lists **Reset All to Default** with its line starting `x`.
10. Drag the Settings scrollbar (`FFScrollbar::track-area`) back to the start of its track (`$M drag FFScrollbar::track-area 0 1193 225`).
   Expect: the headings "PREPARE", "FUSE" and "ALIGN" each show "DEFAULT"; the FUSE "Algorithm" field reads "Details" and the ALIGN "Algorithm" field reads "Pyramidal ECC".
## Algorithm picker
11. Click the **Algorithm** menu under FUSE (`FFMenu::root` 0).
   Expect: a list opens under it with the entries "Details", "Soft transitions" and "Focus map" in that order, their ids "laplacian", "weighted-average" and "depth-map" in grey, and a check mark beside "Details"; the list has 3 entries.
12. Click the entry **Soft transitions**, grey id "weighted-average" (`FFMenuList::option-area` 1).
   Expect: the list is gone; the field reads "Soft transitions"; a row "Sharp-source preference" with a value field reading "4" appears under FUSE; the FUSE heading still shows its "DEFAULT" badge.
13. Click the **More...** bar under the FUSE heading, the second **More...** bar of the panel (`FFMoreToggle::header-area` 1).
   Expect: a row "Radius" appears under "More..." with a value field reading "16".
14. Click the middle of the track of the **Sharp-source preference** slider (`FFSlider::drag-area` 0).
   Expect: the value field reads a number from 4.4 to 4.6 and the FUSE heading has no "DEFAULT" badge.
15. Click the middle of the track of the **Radius** slider (`FFSlider::drag-area` 1).
   Expect: the Radius value field reads "8".
16. Click the **Algorithm** menu under FUSE (`FFMenu::root` 0).
   Expect: the list opens with a check mark beside "Soft transitions" (grey id "weighted-average").
17. Click the entry **Focus map**, grey id "depth-map", the last entry of the list (`FFMenuList::option-area` 2).
   Expect: the field reads "Focus map"; the row "Sharp-source preference" is gone; the rows "Score Radius" (value "4") and "Smoothing Radius" (value "8") are shown under "More...".
18. Click the open **More...** bar under the FUSE heading (`FFMoreToggle::header-area` 1).
   Expect: the rows "Score Radius" and "Smoothing Radius" are gone.
## ALIGN switch and menus
19. Click the **More...** bar under the ALIGN heading, below the ALIGN "Algorithm" field, the third **More...** bar of the panel (`FFMoreToggle::header-area` 2; only two bars exist while the FUSE or PREPARE fold-out is open, so close it first).
   Expect: a row "Mode" appears below that bar, at the bottom edge of the dock, and reads "Rotation + Scale".
20. Click the **Enabled** switch under ALIGN (`FFToggle::area` 0).
   Expect: the switch is off; the ALIGN "Algorithm" label and field, the "More..." bar and the "Mode" label are drawn greyed out; the "Enabled" label is not greyed out.
21. Click the **Algorithm** menu under ALIGN (`FFMenu::root` 1) while ALIGN is switched off.
   Expect: Must not: a list opens under it; the grey id text "pyramidal-ecc" is not shown.
22. Click the **Enabled** switch under ALIGN (`FFToggle::area` 0).
   Expect: the switch is on; the ALIGN "Algorithm" label and field, the "More..." bar and the "Mode" label are no longer greyed out.
23. Drag the Settings scrollbar (`FFScrollbar::track-area`) to the middle of its track (`$M drag FFScrollbar::track-area 0 1193 480`).
   Expect: the ALIGN heading is at the top of the dock with no "DEFAULT" badge (writing the Enabled switch counts as a change, even back to on); the rows "Mode" (reading "Rotation + Scale") and "Max Iterations" (reading "400") are shown.
24. Click the **Mode** field (`FFMenu::root` 1).
   Expect: a list opens under it with the entries "Translation only" and "Rotation + Scale", the grey ids "translation" and "rotation-scale" and a check mark beside "Rotation + Scale".
25. Click **Translation only** (`FFMenuList::option-area` 0).
   Expect: the list is gone; the Mode field reads "Translation only"; the ALIGN heading at the top of the dock has no "DEFAULT" badge.
26. Drag the Settings scrollbar (`FFScrollbar::track-area`) back to the start of its track (`$M drag FFScrollbar::track-area 0 1193 225`).
   Expect: the heading "PREPARE" is visible at the top of the dock.
## More... fold-outs
27. Click the **More...** bar under the PREPARE heading, the first **More...** bar of the panel (`FFMoreToggle::header-area` 0).
   Expect: the rows "Equalize brightness" and "Max brightness gain" appear in the PREPARE section and "Max brightness gain" reads "1.25".
28. Click the open **More...** bar under the PREPARE heading (`FFMoreToggle::header-area` 0).
   Expect: the rows "Equalize brightness" and "Max brightness gain" are gone.
29. Click the open **More...** bar under the ALIGN heading, the third **More...** bar of the panel (`FFMoreToggle::header-area` 2).
   Expect: the row "Mode" is gone.
## Reset All
30. Drag the Settings scrollbar (`FFScrollbar::track-area`) from the top to the end of its track (`$M drag FFScrollbar::track-area 0 1193 705`).
   Expect: the row "Reset All to Default" is visible below the FINISH rows and `buttons FFDock::settings-panel` lists **Reset All to Default** with its line not starting `x`.
31. Click the **Auto-crop** switch (`FFToggle::area` 1) on the first tab.
   Expect: the switch is off and the FINISH heading has no "DEFAULT" badge.
32. Click **Reset All to Default** (`FFResetAll::area`).
   Expect: the Auto-crop switch is on, the FINISH heading shows "DEFAULT" again and `buttons FFDock::settings-panel` lists **Reset All to Default** with its line starting `x`.
33. Drag the Settings scrollbar (`FFScrollbar::track-area`) back to the start of its track (`$M drag FFScrollbar::track-area 0 1193 225`).
   Expect: the headings "PREPARE", "FUSE" and "ALIGN" each show "DEFAULT" and the FUSE Algorithm field still reads "Focus map".
34. Click the second project tab (`FFTab::area` 1).
   Expect: Must not: the FUSE Algorithm field of this tab reads "Focus map" (it reads "Details") or the FINISH heading shows "DEFAULT" (it has no badge).
35. Drag the Settings scrollbar (`FFScrollbar::track-area`) from the top to the end of its track (`$M drag FFScrollbar::track-area 0 1193 705`).
   Expect: the Auto-crop switch is off, the FINISH heading has no "DEFAULT" badge and `buttons FFDock::settings-panel` lists **Reset All to Default** with its line not starting `x`.
