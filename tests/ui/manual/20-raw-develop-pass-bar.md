# Watch the progress bar while a camera-RAW stack is developed, then hand over to alignment
Start: --open .scratch/develop-bar/frames
Before the first step, make a folder of 20 copies of one real camera RAW file (a Fujifilm X-S10 `.raf` of about 19 MB, about 380 MB in all; it is never part of the repository). Take the file from your local camera-sample folder, `$FOCUSFORGE_RAW_SAMPLES`:
`mkdir -p .scratch/develop-bar/frames`
`for i in $(seq -w 1 20); do cp "$FOCUSFORGE_RAW_SAMPLES/fujifilm-x-s10.raf" .scratch/develop-bar/frames/frame-$i.raf; done`
The window then shows one project named "frame-01-20" with 20 frames. A stack of RAW files first develops every frame once, before the stack moves on to alignment. With 20 copies that pass takes from about 10 to 40 seconds, and the bottom bar shows it: the word "Developing", a count "<k> of 20" and a time estimate. The alignment of 20 RAW frames then runs for minutes, so the journey stops it with **Cancel** and never waits for a fused result. Read the bottom bar (`AppWindow::bottom-bar`) with a screenshot right after each step: the develop pass is short.
## The folder before any run
1. Click **Images** (`FFDockTabs::images-tab`).
   Expect: the dock header reads "20 frames · 0 excluded".
2. Click the first frame row (`FFImageRow::area` 0).
   Expect: the left pane label (`FFCanvas::pane-left`) reads "frame-01.raf".
3. Click **Export image** (`FFCheckpointPanel::checkpoint-export`) before any run.
   Expect: Must not: the export dialog opens; `buttons FFDock::checkpoint-panel` lists **Export image** with enabled false.
4. Hover the progress bar (`FFBottomBar::progress-bar`).
   Expect: the status text (`FFBottomBar::status-text`) reads "No job running".
## First run: the develop pass
5. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 30 s) the status text (`FFBottomBar::status-text`) reads "Developing".
6. Hover the progress bar (`FFBottomBar::progress-bar`).
   Expect: the step text (`FFBottomBar::step-text`) reads "<k> of 20" with a whole number k from 0 to 20.
7. Hover the progress bar (`FFBottomBar::progress-bar`).
   Expect: the time text (`FFBottomBar::eta-text`) reads "estimating" or "ETA" followed by minutes and seconds (M:SS), and is not empty.
8. Click the second frame row (`FFImageRow::area` 1) while the run goes on.
   Expect: the left pane label (`FFCanvas::pane-left`) reads "frame-02.raf".
9. Hover the progress bar (`FFBottomBar::progress-bar`).
   Expect: (within 60 s) the step text (`FFBottomBar::step-text`) reads a k above 0 of 20, or the status text already reads "Aligning".
## The hand-over to alignment
10. Hover the progress bar (`FFBottomBar::progress-bar`).
    Expect: (within 120 s) the status text (`FFBottomBar::status-text`) reads "Aligning".
11. Hover the progress bar (`FFBottomBar::progress-bar`).
    Expect: the step text (`FFBottomBar::step-text`) reads "1 of 20", "2 of 20" or "3 of 20": the count starts again from the beginning.
12. Hover the progress bar (`FFBottomBar::progress-bar`).
    Expect: Must not: the status text reads "Developing" again; the time text (`FFBottomBar::eta-text`) reads "ETA" followed by minutes and seconds.
## Stop the first run
13. Click **Cancel** (`FFToolbar::cancel-button`).
    Expect: (within 180 s) the toolbar shows **Fuse stack** and neither **Pause** nor **Cancel** (read with `buttons AppWindow::toolbar`).
14. Hover the progress bar (`FFBottomBar::progress-bar`).
    Expect: the status text (`FFBottomBar::status-text`) reads "No job running".
## Stop a run during its develop pass
15. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 30 s) the status text (`FFBottomBar::status-text`) reads "Developing": the pass starts over, the first run's frames are not reused.
16. Hover the progress bar (`FFBottomBar::progress-bar`).
    Expect: the step text (`FFBottomBar::step-text`) reads "<k> of 20" with a whole number k from 0 to 19.
17. Click **Cancel** (`FFToolbar::cancel-button`) while the status text still reads "Developing".
    Expect: (within 120 s) the toolbar shows **Fuse stack** and neither **Pause** nor **Cancel** (read with `buttons AppWindow::toolbar`); the status text reads "No job running".
18. Click **Projects** (`FFTabStrip::projects-button`).
    Expect: the overlay (`AppWindow::projects-overlay`) shows one card headed "Cancelled" whose stage line reads "STOPPED WHILE DEVELOPING".
19. Click **Projects** (`FFTabStrip::projects-button`) again.
    Expect: the overlay is gone and the toolbar shows **Fuse stack** (`FFToolbar::fuse-button`).
## A last look at the develop pass
20. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 30 s) the status text (`FFBottomBar::status-text`) reads "Developing", the step text (`FFBottomBar::step-text`) reads "<k> of 20" and the time text (`FFBottomBar::eta-text`) reads "estimating" or "ETA" followed by minutes and seconds.
