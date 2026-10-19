# Move around the window: dock tabs, projects, zoom, overlay and pane windows
Start: --open fixtures/mantis-64/frames --open fixtures/mantis-512/frames
## Dock tabs and frame selection
1. Click the first tab (`FFTab::area` 0).
   Expect: the tab strip shows two tabs and the dock header reads "3 frames · 0 excluded".
2. Click **Settings** (`FFDockTabs::settings-tab`).
   Expect: the dock shows a list of parameter rows including a row labelled "Algorithm"; the text "3 frames" is not visible.
3. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the dock header reads "3 frames · 0 excluded" and the list shows 3 rows.
4. Click the second frame row (`FFImageRow::area` 1).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "01.tif".
## Two projects keep their own state
5. Click the second tab (`FFTab::area` 1).
   Expect: the dock header reads "8 frames · 0 excluded" and the left pane label reads "00.tif".
6. Click the first tab (`FFTab::area` 0).
   Expect: the dock header reads "3 frames · 0 excluded" and the left pane label reads "01.tif".
7. Click the first frame row (`FFImageRow::area` 0).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "00.tif".
## Zoom and its limits
8. Click the second tab (`FFTab::area` 1).
   Expect: the dock header reads "8 frames · 0 excluded"; the zoom steps below are on this 512-pixel project, whose fit is below 500 %.
9. Click **Fit** (`FFZoomCluster::zoom-fit`).
   Expect: the zoom readout (`FFZoomCluster::zoom-readout`) shows a percentage ending in " %"; call it the fit value.
10. Click **Zoom in** (`FFZoomCluster::zoom-in`).
    Expect: the zoom readout shows a percentage higher than the fit value.
11. Click **Zoom in** (`FFZoomCluster::zoom-in`).
    Expect: the zoom readout shows a percentage higher than after step 10.
12. Click **Zoom out** (`FFZoomCluster::zoom-out`).
    Expect: the zoom readout shows a percentage lower than after step 11.
13. Click **Fit** (`FFZoomCluster::zoom-fit`).
    Expect: the zoom readout shows the same percentage as the fit value of step 9.
14. Click **Zoom out** (`FFZoomCluster::zoom-out`).
    Expect: the zoom readout shows a percentage lower than the fit value; call it the minimum.
15. Click **Zoom out** (`FFZoomCluster::zoom-out`) again.
    Expect: Must not: the zoom readout drops below the minimum of step 14; it shows the same percentage.
16. Click **Fit** (`FFZoomCluster::zoom-fit`).
    Expect: the zoom readout shows the fit value of step 9 again.
## The settings window blocks the main window
17. Click **Settings** (`FFIconBar::settings`, icon bar at the bottom-left).
    Expect: a window titled "Settings" opens (`AppWindow::settings-modal`) with six rail entries Performance, Appearance, Storage, Input map, Diagnostics and About.
18. Click **Zoom in** (`FFZoomCluster::zoom-in`) while the Settings window is open.
    Expect: Must not: the zoom readout behind the window rises above the fit value; it still shows the fit value of step 9.
19. Click **Close settings** (`FFSettingsModal::close`).
    Expect: the "Settings" window and its rail are gone; the canvas panes are visible and the zoom readout shows the fit value.
## Must not work: the overlay before a fuse
20. Click the **Overlay** toggle (`FFToggle::area` 0, the one at the right of the bottom bar).
    Expect: Must not: the toggle turns on; the knob stays on the left with no accent fill and the toggle looks dimmed.
21. Drag the overlay slider (`FFSlider::drag-area` 0, the only slider in the window) to window point 1120,678.
    Expect: Must not: the percentage beside the slider changes; it still reads "65%" and the slider looks dimmed.
## Fuse and keep
22. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 60 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep".
23. Click **Settings** (`FFDockTabs::settings-tab`) while the dialog is open.
    Expect: Must not: the dock changes; it shows no row labelled "Algorithm" and the dialog "Keep this fused result?" is still shown.
24. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone, the bottom bar status reads "fused" followed by a time (for example "fused 16:34") and the right pane shows a filled composite.
25. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: the stacking toolbar shows a **Fuse stack** button (read with `buttons AppWindow::toolbar`) and the canvas shows the frame pane and the Composite pane.
## The overlay after a fuse
26. Click the **Overlay** toggle (`FFToggle::area` 0, the one at the right of the bottom bar).
    Expect: the toggle is on (knob on the right, accent fill) and the percentage beside the slider reads "65%" at full strength.
27. Drag the overlay slider (`FFSlider::drag-area` 0, the only slider in the window) to window point 1120,678.
    Expect: the percentage beside the slider reads a value above 65% and coloured bands tint the Composite pane (`FFCanvas::pane-right`); the frame pane (`FFCanvas::pane-left`) has no tint.
28. Click the **Overlay** toggle (`FFToggle::area` 0, the one at the right of the bottom bar).
    Expect: the toggle is off, the overlay slider is greyed out and the bands are gone from the Composite pane.
## A pane in its own window
29. Click the undock control of the left pane (`FFUndockControl::area` 0).
    Expect: (within 5 s) the main window shows one pane, labelled "Composite", across the whole canvas and no pane labelled "00.tif"; the new window (`$M shot <png> N --scale 0.5` with N the last entry of `list_windows`, counted from 0) has the title bar "FocusForge — Frame" and the pane label "00.tif".
30. Click **Fit** (`FFZoomCluster::zoom-fit`).
    Expect: the main window still shows one pane, labelled "Composite", and the zoom readout shows a percentage ending in " %".
31. Click **Settings** (`FFDockTabs::settings-tab`).
    Expect: Must not: the Composite pane in the main window shows an undock control in its top-right corner (the dock does show a row labelled "Algorithm").
32. Click the close control in the second window's title bar (`FFWindowBar::close`).
    Expect: the second window is gone; the main window shows two panes, "00.tif" on the left and "Composite" on the right, each with an undock control.
33. Click the undock control of the left pane (`FFUndockControl::area` 0).
    Expect: (within 5 s) a new second window titled "FocusForge — Frame" opens (`$M shot <png> N --scale 0.5` with N the last entry of `list_windows`, counted from 0) and the main window shows one pane, "Composite".
34. Click the dock control (label "Dock pane", `FFUndockControl::area` 0) in the second window (`--window N`, N the last entry of `list_windows`, counted from 0; the id is in both windows).
    Expect: the second window is gone and the main window shows two panes again, "00.tif" and "Composite".
