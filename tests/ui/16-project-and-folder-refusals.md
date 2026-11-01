# Open folders and projects that are refused, mismatched, missing or grouped, then fuse a stack that fails
Start: --open fixtures/open-rejected --open fixtures/open-root-and-subfolder --open fixtures/projects/missing.ffp --open fixtures/projects/mismatch.ffp --open fixtures/open-two-stacks --open fixtures/projects/corrupt.ffp
## Four dialogs stand stacked, dismissed from the top
1. Click **Projects** (`FFTabStrip::projects-button`) while the launch's dialogs are up.
   Expect: (within 30 s) Must not: the Projects overview opens; a dialog titled "This project cannot be opened" with the text "This file is not a FocusForge project." and a **Close** button stays on top.
2. Click **Close** by its id (`FFRefusalDialog::pick-area`; it is not listed by label).
   Expect: that dialog is gone and a dialog titled "The frames are no longer there" shows the name "missing" and the text "3 of 3 cannot be found in this location."; its footer buttons read "Close" and "Locate frames…", beside a **Close dialog** icon button at its top-right corner (`buttons AppWindow::sources-missing-dialog` lists all three).
3. Click **Close dialog** (`AppWindow::sources-missing-dialog`), the icon button at the dialog's top-right corner.
   Expect: that dialog is gone and a dialog titled "The source frames are not the same" shows the name "mismatch.ffp" with the buttons "Cancel", "Locate frames…" and "Open anyway", beside a **Close dialog** icon button at its top-right corner.
4. Click **Open anyway** (`AppWindow::mismatch-dialog`).
   Expect: that dialog is gone and a question reads "This folder reads as 2 stacks of 6 frames." with the buttons "Open as one project", "Open as 2 projects" and "Cancel", beside a **Close dialog** icon button at its top-right corner.
5. Click **Projects** (`FFTabStrip::projects-button`) while the question is up.
   Expect: Must not: the Projects overview opens; the question "This folder reads as 2 stacks of 6 frames." stays on screen.
6. Click **Open as 2 projects** (`FFGroupingQuestion::many-button`).
   Expect: (within 30 s) the question is gone, no dialog is on screen, a tab named "a" is selected and the dock header reads "3 frames · 0 excluded".
## The projects overview lists what opened and what did not
7. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overview shows the heading "Projects" and the count "7 · 1 did not open"; the cards "open-rejected" and "open-root-and-subfolder" read "3 frames · no checkpoint yet"; a card headed "Sources missing" with the button "Locate frames…" sits above the card "missing" ("3 frames · 1 checkpoint"), beside the card "mismatch" ("3 frames · 1 checkpoint").
8. Drag the gallery scrollbar (`FFScrollbar::track-area`, instance 1) to window point 1189,750.
   Expect: the cards "a" and "b" read "3 frames · no checkpoint yet" and the last card is headed "Not a project" with the line "LEFT ALONE" and the name "notes"; no card is named "corrupt".
9. Click **Projects** (`FFTabStrip::projects-button`).
   Expect: the overview is gone and the toolbar shows **Fuse stack**.
## The skipped subfolder is recorded
10. Click **Notifications** (`FFIconBar::bell`).
    Expect: the center opens reading "1 notification · this session" with one row titled "Some images were skipped" and the line "Import"; the bell shows no number.
11. Click that row (`FFNotificationRow::card`, instance 0).
    Expect: the row shows the message "1 subfolder skipped:" followed by a path ending in "open-root-and-subfolder/notes".
12. Click **Notifications** (`FFIconBar::bell`).
    Expect: the center is gone.
## Each opened tab
13. Click the tab "missing" (`FFTab::area`, instance 2).
    Expect: Must not: a dialog is on screen; the first row of the Images list reads "Source missing" and the left pane reads "Cannot show this frame:".
14. Click the tab "mismatch" (`FFTab::area`, instance 3).
    Expect: (within 5 s) the dock header reads "3 frames · 0 excluded" and the first row of the Images list shows a picture, not "Source missing".
15. Click the tab "open-root-and-subfolder" (`FFTab::area`, instance 1).
    Expect: the dock header reads "3 frames · 0 excluded" and the Images list shows the rows 00.tif, 01.tif and 02.tif.
16. Click the left arrow of the tab strip (`FFTabStrip::left-arrow`).
    Expect: the tab "open-rejected" is visible at the left end of the tab strip.
17. Click the tab "open-rejected" (`FFTab::area`, instance 0).
    Expect: the dock header reads "3 frames · 0 excluded", the row 02.tif reads "Preview unavailable" and the toolbar shows **Fuse stack**.
## A stack the run rejects fails and fails again on Retry
18. Click **Fuse stack** (`FFToolbar::fuse-button`).
    Expect: (within 30 s) the bottom bar status reads "Failed", the toolbar button reads "Retry" (`FFToolbar::retry-button`) and the bell shows the number 1; Must not: the dialog "Keep this fused result?" appears.
19. Click **Retry** (`FFToolbar::retry-button`).
    Expect: (within 30 s) the bottom bar status reads "Failed", the toolbar button still reads "Retry" and the bell shows the number 2; Must not: a composite is filled or the dialog "Keep this fused result?" appears.
20. Click **Notifications** (`FFIconBar::bell`).
    Expect: the center reads "3 notifications · this session" and lists two rows titled "Fusion failed" above the row "Some images were skipped".
21. Click **Notifications** (`FFIconBar::bell`).
    Expect: the center is gone, **Retry** is still shown and no dialog is on screen.
