# UI journeys

A journey is one whole user path through the window, written as numbered clicks. Each step names one action (Click or Drag), the visible label and the element id in backticks, and one `Expect:` line with something a screenshot shows: text, enabled or greyed, present or gone, a count. A journey holds both what must work and what must not (greyed controls, refusals, cancel).

## Format

```
# Title
Start: <arguments for focusforge-gui>
## Section
1. Click **Label** (`Component::id`).
   Expect: what the window shows.
```

- 20 to 35 steps per journey; files are named `NN-<flow>.md`.
- `Start:` uses inputs from `fixtures/` only.
- A step that cannot be driven by clicks (typing, wheel, native file dialog) starts with `(human only)`.
- An id with an instance number (`FFSlider::drag-area`, instance 1) is the second element of that id in the window's tree order.

## Run one by hand

1. Build: `cargo build -p focusforge-gui`.
2. Start the window with the journey's `Start:` arguments and a throwaway settings folder: `target/debug/focusforge-gui <Start args> --config-dir <empty folder>`.
3. Do the steps in order and compare each screen with its `Expect:`. A step whose result does not appear is a failed step; carry on with the next one.

The `.slint` sources under `crates/gui/ui` hold every id.
