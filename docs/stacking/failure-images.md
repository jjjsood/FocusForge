# Common failure images

Most stacks that go wrong go wrong in one of three ways: a glow along the
edges of the subject, doubled or broken outlines, or soft bands between sharp
ones. Each has a cause you can name, a way to avoid it next time you shoot,
and often a way to repair it in FocusForge.

For each failure this chapter shows an example, then answers four questions:
what you see, why it happens, how to avoid it when shooting, and how to
repair it.

The examples are 1:1 crops (one image pixel per screen pixel) fused by
FocusForge from its sample stack: eight frames, numbered 00 to 07 in focus
order, taken from a series of a praying mantis's head. The
[previous chapter](depth-of-field.md#an-example) introduces them.

> **Tip:** Judge a composite at 100 %. All three failures are easy to miss
> in a small preview. In the window, the bottom bar's **Overlay** colours the
> **Composite** pane by the frame each area came from; it is the quickest way
> to see where a soft band or a halo was taken from (see
> [Retouching](../guide/retouch.md)).

## Halos at depth edges

HALO_IMAGE

### What you see

A pale or dark fringe, or a soft ghost outline, running alongside an edge
where a sharp foreground object stands in front of a more distant background:
around antennae, hairs, the edges of legs or petals. The object itself is
sharp; the strip of background next to it looks smeared or glowing.

### Why it happens

A **[halo](../glossary.md#halo)** comes from the optics, not from a bug. In
the frames focused on the foreground object, the background behind it is
blurred. In the frames focused further back, the foreground object is out of
focus and its blur spreads out over the background beside it. Next to the
edge there is therefore no frame in which the background is both sharp and
free of the foreground's blur. The fusion method has to choose between
blurred versions, and a broad, soft edge can score as "detail" too.

The methods handle this differently. A method that picks one frame per area
keeps the strip next to the edge from a single frame; a method that blends
mixes the soft spread of several frames into it.

### How to avoid it when shooting

- Give the subject distance from the background. The further away the
  background, the smoother and more even its blur, and the less there is to
  confuse.
- Prefer a plain, evenly lit background to a busy one.
- Keep the steps between frames small enough to overlap (see
  [Shooting a stack](shooting-practice.md)). This does not remove halos, but
  it keeps the edge itself clean.

### How to repair it

1. Fuse again with another fusion method and compare. The **Algorithm**
   picker in the dock's **FUSE** settings offers **Details**,
   **Soft transitions** and **Focus map**; see
   [Stacking settings](../guide/stacking.md).
2. Retouch what remains: in the editor, pick a frame in which the
   background next to the edge looks clean and paint it in with the brush.
   See [Retouching](../guide/retouch.md).

## Motion between frames

MOTION_IMAGE

### What you see

Outlines that appear twice, a sharp edge with a soft copy just beside it, or
a structure that does not line up with itself: an antenna with a kink or a
step where one frame takes over from the next. When the whole picture shows
it, the camera moved. When only one part does, the subject moved.

### Why it happens

Focus stacking assumes that only the focus changes between frames. If the
camera moves, every frame after the movement shows the scene in a slightly
different place. If the subject moves, a leg, an antenna or a leaf is in a
different place in some frames while everything else stays put. Either way,
the composite takes each area from its sharpest frame, and areas taken from
frames on either side of the movement no longer fit together.

**Camera movement** comes from knocking the tripod, the shutter, a loose
focusing rail, or a support that settles. **Subject movement** comes from
wind, a living subject, or a subject settling on its perch.

### How to avoid it when shooting

A steady support, a remote release or self-timer, a pause after every step
for vibration to die down, and shelter from wind. Living subjects need short
series or calm moments. All of this is covered in
[Shooting a stack](shooting-practice.md).

### How to repair it

- **Camera movement:** alignment repairs it. It is on by default: FocusForge
  registers every frame onto a reference frame, shifting, rotating and
  scaling it as needed, and crops the result to the area every frame covers.
  See [Alignment](../guide/alignment.md), which also explains how to see
  which frames registered and which did not.
- **Subject movement:** alignment cannot help, because it moves whole frames
  and the rest of the scene did not move. Instead:
  1. Find the frames in which the subject moved: select them one after the
     other in the [Images panel](../guide/images-panel.md) and compare them
     in the frame pane beside the **Composite** pane.
  2. **Exclude** them in the [Images panel](../guide/images-panel.md) and fuse
     again. An excluded frame stays in the list and can be included again.
  3. Repair what is left with the brush, painting the area from one frame in
     which it is sharp. See [Retouching](../guide/retouch.md).
- If the movement spans the frames that hold a part of the subject sharp,
  there is nothing to repair from: shoot that part again.

## Focus gaps

GAP_IMAGE

### What you see

Soft bands or patches between areas that are sharp. On a surface that runs
away from the camera, such as a leaf, a wing or the side of a head, they
show up as stripes: sharp, soft, sharp, soft.

### Why it happens

A **[focus gap](../glossary.md#focus-gap)** is a slice of the subject that is
not sharp in any frame. Either the step between two frames was larger than
the depth of field, so their sharp slices did not overlap, or frames are
missing from the series: deleted, skipped, or excluded before fusing. Focus
stacking can only pick from what the frames contain, so a slice that no frame
holds sharp stays soft.

### How to avoid it when shooting

Choose a step size smaller than the depth of field, so neighbouring frames
overlap, and check the series for missed frames before you pack up. See
[Shooting a stack](shooting-practice.md).

### How to repair it

- If the frames exist but were left out, put them back: **Include** an
  excluded frame in the [Images panel](../guide/images-panel.md) (or add the
  missing files) and fuse again.
- If the frames were never taken, no setting or retouch can recover the
  detail; retouching can only paint from frames you have. Shoot the missing
  range again, ideally the whole series.

## Where to go next

- [Shooting a stack](shooting-practice.md): how to avoid all three before
  they happen.
- [Stacking settings](../guide/stacking.md): the fusion methods and their
  settings.
- [Retouching](../guide/retouch.md): painting areas of the composite from a
  chosen frame.
