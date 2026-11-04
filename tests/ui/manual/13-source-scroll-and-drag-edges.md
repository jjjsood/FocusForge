# Source list scrolling, keyboard and drag edges over a long list
Start: --open .scratch/source-scroll/frames
Before the first step, make a folder of 40 frames, one of them with a file name long enough to clip in a row of every presentation (`fixtures/mantis-512/frames` holds 8 frames; its files are copied under 40 names):
`mkdir -p .scratch/source-scroll/frames`
`for i in $(seq -w 1 40); do cp fixtures/mantis-512/frames/0$((10#$i % 8)).tif .scratch/source-scroll/frames/frame-$i.tif; done`
`mv .scratch/source-scroll/frames/frame-40.tif .scratch/source-scroll/frames/frame-40-a-file-name-long-enough-to-clip-in-every-row.tif`
The Images list then scrolls in List, Image list and Large previews. Pick the presentation under Settings > Appearance (Presentation, `FFMenu::root`) and close the Settings window with **Close settings** (`FFSettingsModal::close`); reopen it with the Settings icon in the icon bar (bottom-left). These steps need the mouse wheel, held mouse buttons and the window's own keyboard focus, so a person runs them.
## Must not work
1. (human only) Start a filename drag, observe its row following the pointer, leave the source viewport, and release.
   Expect: Must not: a drag remains active after release or a later pointer move reorders sources.
## Selection visibility
2. (human only) In List, Image list and Large previews, wheel over a source row until selection leaves the initial viewport, then wheel back. Hover the clipped filename in each presentation.
   Expect: the canvas source changes with each wheel direction and the selected row remains visible in the source panel. The full clipped filename appears in the floating tooltip; moving to a short filename or leaving the row hides it.
## Keyboard and drag visibility
3. (human only) In List, Image list and Large previews, focus a source row, press Down until selection leaves the initial viewport, then press Up back to the first source.
   Expect: each key changes the canvas source and keeps its selected source row visible; the first and last source stay visible when their direction key is pressed at the edge.
4. (human only) In each presentation, drag a source to the top viewport edge and hold, then release it; repeat at the bottom edge. Start a press near an edge without moving, then release.
   Expect: a moved held drag auto-scrolls only within the source extent, releases with the moved source fully visible, and stops scrolling on release. A stationary edge press neither scrolls nor reorders sources.
