# Brightness equalization target
Start: --open fixtures/mantis-512/frames --dock-tab settings

Use a fresh settings directory. The PREPARE section of the Settings dock holds the Equalize brightness switch, the Max brightness gain slider and the Target brightness (stops) slider. With only PREPARE's More... open, the sliders in the dock are `FFSlider::drag-area` instance 0 (Max brightness gain) and instance 1 (Target brightness); instance 2 is the Overlay slider in the bottom bar. The Equalize brightness switch is `FFToggle::area` instance 0. The target readout is signed: a minus is the character U+2212 (`−0.5`), zero is `0.0`, a positive value carries a plus (`+0.5`). For every drag on a slider, read its track with `get_element_properties` immediately before: the left end is absolutePosition.x, the right end is absolutePosition.x + width, and the pointer height is absolutePosition.y + 10. Use these logical coordinates as they are, never multiplied by the window scale. A drop 40 px left of the left end or 40 px right of the right end sets the slider to its minimum or maximum. A drop at 37% is x = absolutePosition.x + 0.37 × width. The Fuse stack run on this input takes several seconds the first time its target changes, and the bottom bar may read Preparing for under a second before it reads Aligning. Pause has to be clicked within a few seconds of Fuse stack: if the run has already ended, the dialog "Keep this fused result?" is showing instead of Resume, and steps 16 to 18 find no paused run.

## Open the PREPARE drawer
1. Click **More...** under PREPARE (`FFMoreToggle::header-area`, instance 0).
   Expect: the PREPARE section lists Equalize brightness, Max brightness gain reading 1.25 and Target brightness (stops) reading 0.0.
2. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the dock reads "8 frames · 0 excluded" above a list starting with 00.tif and shows no PREPARE rows.
3. Click **Settings** (`FFDockTabs::settings-tab`).
   Expect: the Equalize brightness switch is on (orange); Max brightness gain reads 1.25 and Target brightness reads 0.0.

## Keyboard and snapping
4. Click **Target brightness track** (`FFSlider::drag-area`, instance 1).
   Expect: the target readout reads 0.0 and has an orange border.
5. Press Right once.
   Expect: the target readout reads +0.1 and keeps its orange border.
6. Press Left three times.
   Expect: the target readout reads −0.2.
7. Drag **Target brightness track** (`FFSlider::drag-area`, instance 1) to its 37% point.
   Expect: the target readout reads −0.1, not −0.13.
8. Drag **Target brightness track** (`FFSlider::drag-area`, instance 1) to 40 px right of its right end.
   Expect: the target readout reads +0.5 and the thumb sits at the right end.
9. Press Right once more (the Target brightness slider keeps the focus from step 8).
   Expect: the target readout still reads +0.5.
10. Drag **Target brightness track** (`FFSlider::drag-area`, instance 1) to 40 px left of its left end.
   Expect: the target readout reads −0.5 and the thumb sits at the left end.
11. Click **Target brightness track** (`FFSlider::drag-area`, instance 1).
    Expect: the target readout reads 0.0.
12. Drag **Max brightness gain track** (`FFSlider::drag-area`, instance 0) to its 37% point.
    Expect: the max gain readout reads 1.38, with no plus sign.
13. Drag **Target brightness track** (`FFSlider::drag-area`, instance 1) to 40 px left of its left end.
    Expect: the target readout reads −0.5.

## Hold a run
14. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: Check at end of section.
15. Click **Pause** (`FFToolbar::pause-button`) straight after step 14, with no screenshot in between.
    Expect: Check at end of section.
16. Click **Target brightness track** (`FFSlider::drag-area`, instance 1).
    Expect: Must not: the target readout changes; it still reads −0.5 while the bottom bar reads Paused, Resume replaces Pause in the toolbar and the Max brightness gain and Target brightness rows and the More... bar are greyed.
17. Click **Equalize brightness switch** (`FFToggle::area`, instance 0).
    Expect: Must not: the switch turns off; it stays on.

## Finish the run
18. Click **Resume** (`FFToolbar::resume-button`).
    Expect: the dialog "Keep this fused result?" appears (within 60 s).
19. Click **Keep** (`AppWindow::divergence-warning`).
    Expect: the dialog is gone; the bottom bar reads "fused" followed by a time.
20. Click **STACKING** (`FFModeSwitch::stacking-half`).
    Expect: Fuse stack shows in the toolbar; the target readout reads −0.5 and the PREPARE rows are no longer greyed.

## Equalize off keeps the values
21. Click **Equalize brightness switch** (`FFToggle::area`, instance 0).
    Expect: the switch is off; Max brightness gain and Target brightness are greyed and still read 1.38 and −0.5.
22. Click **Target brightness track** (`FFSlider::drag-area`, instance 1).
    Expect: Must not: the target readout changes; it still reads −0.5.
23. Click **Equalize brightness switch** (`FFToggle::area`, instance 0).
    Expect: the switch is on; both slider rows are no longer greyed and read 1.38 and −0.5.

## No project
24. Click **close tab** (`FFTab::close-area`).
    Expect: the dialog "Save this state first?" appears with Cancel, Save and close and Close anyway.
25. Click **Cancel** (`AppWindow::close-guard`).
    Expect: Must not: the project closes; the dialog is gone and the frames tab is still open.
26. Click **close tab** (`FFTab::close-area`).
    Expect: the dialog "Save this state first?" appears.
27. Click **Close anyway** (`AppWindow::close-guard`).
    Expect: the frames tab is gone; the dock reads "No project open." and no Equalize brightness or Target brightness row is visible.
28. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: Must not: a run starts; Fuse stack stays greyed and the bottom bar reads "No project open".
