# UI test journeys

A journey is one whole user path through the FocusForge window, written as 20 to 35 numbered clicks. Each click has one result you can see in a screenshot, so a person and a script reach the same verdict. Every journey covers what works and what must not work (refusals, greyed controls, guards, cancel paths) in the same file.

## Format

```
# Fuse a stack for the first time
Start: --open fixtures/mantis-64/frames
## Fuse and keep
1. Click **Fuse stack** (`FFToolbar::fuse-button`).
   Expect: (within 60 s) a dialog titled "Keep this fused result?" shows two buttons, "Discard" and "Keep".
## Must not work
2. Click **Export** (`FFCheckpointPanel::checkpoint-export`) before any fuse.
   Expect: Must not: the export dialog opens; **Export** is greyed out.
```

- `# Title` names the path; the file is `NN-<flow>.md`.
- `Start:` holds the arguments for `focusforge-gui`. Inputs come from `fixtures/` only.
- `## Section` lines group steps. Steps are numbered across the whole file; each is one action with the visible label and the element name in backticks (`Component::id`): `Click`, `Drag` (press on the element, move to a window point, release), `Hover` (put the pointer on the element, no click), `Type "text" into` a field (click the field, then type the text), `Alt+click` (click with Alt held) or `Press <Key>` (a key such as Escape, Return, Down, Left or Right, sent to the control that holds the keyboard focus; the name may be left out).
- An id followed by a number (`FFSlider::drag-area` 1, also written "instance 1" or "match 1") is that element of the id in the window's tree order, counted from 0.
- Each step has one indented `Expect:` line naming something visible: text, enabled or greyed, present or gone, a count. `(within N s)` allows a wait. `Expect: Must not: …` marks a step that checks a refusal. `Expect: Check at end of section.` skips the check for a setup step; the section's last step carries it.
- A step that says "in the second window" acts on that window, not the main one (a script passes `--window N`, counted from 0 in the order the windows were opened).
- A step that needs the window manager, a native dialog, the mouse wheel or the window's own keyboard focus starts with `(human only)`.
- A dialog blocks everything under it, so a journey closes it with its own step.
- A journey is edited when the app changes, never split into small tests. Every journey is listed in a feature file under `features/`.

## Run one by hand

1. Build the app, create a scratch save folder, then start it with the journey's `Start:` arguments, a scratch settings folder and that save folder (saves and exports land there with no file chooser, never beside the fixtures):
   `mkdir -p .scratch/ui/save`
   `target/debug/focusforge-gui <Start args> --config-dir .scratch/ui/config --save-folder .scratch/ui/save`
2. Do the steps in order. After each step compare the window with its `Expect:`.
3. A step whose `Expect:` does not hold is a failure; note its number and what the window showed, then carry on with the next step.
4. Close the window when done.
