# Depth of field and why stack

Take a close-up of an insect and look at it full size: one thin slice is
crisp, and everything in front of it and behind it melts into blur. That is
not a fault of your lens or your technique. It is what optics does at short
distances, and it is the reason focus stacking exists.

This chapter explains where that thin slice comes from, why it gets so thin
in macro work, and what focus stacking does about it. The next chapter,
[Shooting a stack](shooting-practice.md), turns this into practice, and
[Common failure images](failure-images.md) shows what goes wrong and how to
fix it.

## What depth of field is

A lens focuses perfectly on one plane only. A point exactly on that plane
lands on the sensor as a point. A point a little in front of it or behind it
lands as a small disc of light, a blur disc, and the further the point is
from the plane of focus, the bigger its disc.

As long as a blur disc is too small for you to tell it from a point, that
part of the picture looks sharp. The **[depth of field](../glossary.md#depth-of-field)**
is the zone, in front of and behind the plane of focus, in which the blur
stays that small.

The largest blur disc that still counts as sharp is called the
**[circle of confusion](../glossary.md#circle-of-confusion)**. It is a
convention, not a law of nature. Common values, for a whole picture viewed at
a normal size:

| sensor | circle of confusion |
|---|---|
| full frame (36 × 24 mm) | 0.03 mm |
| APS-C | about 0.02 mm |
| Micro Four Thirds | about 0.015 mm |

> **Note:** Depth of field has no hard edge. Sharpness falls off gradually on
> both sides of the plane of focus. If you look at a picture at 100 % on
> screen, or print it large, you see smaller blur discs than the convention
> assumes, and the zone that *looks* sharp gets thinner.

## What makes it deep or shallow

Three things set the depth of field:

| factor | deeper zone | shallower zone |
|---|---|---|
| **Aperture** (the f-number) | smaller aperture, larger f-number (f/11) | wider aperture, smaller f-number (f/2.8) |
| **Magnification** (how large the subject is on the sensor) | subject small on the sensor | subject large on the sensor |
| **Sensor and viewing size** (through the circle of confusion) | small prints, the whole frame on screen | large prints, 100 % crops |

**[Magnification](../glossary.md#magnification)** is the size of the subject's
image on the sensor divided by the subject's real size. A lens at 1:1 ("life
size") puts a 10 mm beetle on the sensor as a 10 mm image; at 1:2 the image is
5 mm. Magnification is what you change when you move closer or use a longer
lens, and in macro work it is by far the strongest of the three factors.

> **Tip:** At the same magnification, the focal length barely changes the
> depth of field. A 60 mm and a 100 mm macro lens at 1:1 give practically
> the same depth of field; the longer lens just gives you more working
> distance.

## A worked example: why it collapses at macro distances

For close-up work, a good approximation of the total depth of field is:

```text
depth of field ≈ 2 · N · c · (1 + m) / m²
```

- `N` is the f-number set on the lens (for example 8 for f/8).
- `c` is the circle of confusion in millimetres (0.03 for full frame).
- `m` is the magnification (1 for 1:1, 0.5 for 1:2, 2 for 2:1).

Read the formula from the outside in. The f-number is on top, so stopping
down deepens the zone in proportion: f/16 gives twice the depth of f/8. The
magnification is squared underneath, so it dominates: double the
magnification and the depth shrinks to roughly a quarter.

Work it through for a full-frame camera at f/8 and 1:1:

```text
2 × 8 × 0.03 mm × (1 + 1) / 1² = 0.96 mm
```

About **one millimetre**. Everything sharp in that picture lies inside a
slice one millimetre thick. The same camera and aperture at other
magnifications:

| magnification | area the full frame covers | depth of field at f/8 |
|---|---|---|
| 1:10 (0.1×) | 360 × 240 mm | about 53 mm |
| 1:4 (0.25×) | 144 × 96 mm | about 9.6 mm |
| 1:2 (0.5×) | 72 × 48 mm | about 2.9 mm |
| 1:1 (1×) | 36 × 24 mm | about 0.96 mm |
| 2:1 (2×) | 18 × 12 mm | about 0.36 mm |
| 5:1 (5×) | 7.2 × 4.8 mm | about 0.12 mm |

The head of a praying mantis, like the one in the examples below, is several
millimetres deep. At 1:1 a single frame covers about a millimetre of it. No
single exposure can show the whole head sharp.

> **Note:** The `(1 + m)` term is there because a lens focused close lets
> less light through than its f-number says. The *effective* aperture is
> about `N × (1 + m)`: f/8 at 1:1 behaves like f/16, and f/8 at 5:1 like
> f/48. This matters for the next section.

## Why stopping down is not the answer

The formula suggests a simple fix: close the aperture until everything is
sharp. In practice that fails because of
**[diffraction](../glossary.md#diffraction)**. The smaller the aperture, the
more light spreads as it passes the aperture's edge, and every point in the
picture, including the ones in focus, turns into a larger soft spot. At macro
magnifications the effective aperture is what counts, so you reach that
limit early: past a certain f-number you buy a deeper zone with a softer
picture overall.

How to choose a working aperture is covered in
[Shooting a stack](shooting-practice.md).

## What focus stacking does

**[Focus stacking](../glossary.md#focus-stacking)** gets around the limit by
taking many pictures instead of one. Each picture is sharp in its own thin
slice, and the software assembles the sharp parts into one image, the
**[composite](../glossary.md#composite)**.

1. **Shoot a series.** Keep the camera and the subject still and move the
   plane of focus a small step between frames, from the nearest part of the
   subject to the farthest. The sharp slices of neighbouring frames must
   overlap, so that every part of the subject is sharp in at least one frame.
2. **Line the frames up.** Moving the focus changes the magnification a
   little (focus breathing), and the camera may shift slightly between
   frames. FocusForge's **[alignment](../glossary.md#alignment)** registers
   every frame onto a reference frame and crops the result to the area every
   frame covers. See [Alignment](../guide/alignment.md).
3. **Score sharpness everywhere.** For every pixel of every frame, FocusForge
   measures how much fine detail sits around it: how strongly the brightness
   changes from one pixel to the next over a small neighbourhood. In-focus
   areas are full of such changes; out-of-focus areas are smooth.
4. **Pick or blend.** For every pixel, the composite takes its value from the
   frame that scored sharpest there, or blends several frames weighted by
   their scores. The rule for this is the
   **[fusion method](../glossary.md#fusion-method)**; FocusForge has three,
   **Details**, **Soft transitions** and **Focus map**, described in
   [Stacking settings](../guide/stacking.md).
5. **Keep the record.** FocusForge stores which frame each pixel came from as
   the **[Z-Map](../glossary.md#z-map)**. You can show it over the composite
   and use it when you retouch an area by hand (see
   [Retouching](../guide/retouch.md)).

### An example

The pictures below are 1:1 crops (one image pixel per screen pixel) of one
window of a 61-frame series of a praying mantis's head, shot with the focus
moving from front to back. The examples in this book use eight of those
frames, spread evenly over the series and numbered 00 to 07 in focus order.

![Three crops of the same window of the mantis's head, side by side. Left: the antenna is sharp, the rim of the head is soft. Middle: the rim of the head and its fine hairs are sharp, the antenna is a broad soft band. Right: nothing in the window is sharp.](../assets/manual/depth-of-field/source-frames.jpg)

*Single frames 02, 03 and 05 of the eight: frame 02 is sharp on the antenna, frame 03 on the rim of the head, frame 05 is focused behind the head.*

Each frame is sharp in a different band. In frame 02 the antenna stands out
and the head behind it is soft; one step further, in frame 03, the rim of the
head and its hairs are sharp and the antenna has dissolved into a pale band.
By frame 05 the focus has moved past the head altogether.

![Two images side by side. Left: the composite, with the antenna and the hairs on the rim of the head both sharp. Right: the same window coloured by source frame: orange along the antenna, blue along the rim of the head and over much of the rest, with grey patches.](../assets/manual/depth-of-field/composite.jpg)

*Left: the FocusForge composite of all eight frames (Details method, default settings). Right: its Z-Map, orange where a pixel came from frame 02, blue for frame 03, grey for any of the other six frames.*

The composite has the antenna from frame 02 and the rim from frame 03, both
sharp, in one picture. The Z-Map on the right shows how it was assembled: the
antenna is orange, the rim and its hairs blue. In the smooth background and
on the soft surface of the head, where no frame has real detail, the choice
of frame hardly matters, and the map there is a patchwork of frame 03 and
other frames.

Look closely where the antenna crosses the rim: there the sharp rim won, and
the antenna looks soft and see-through for a short stretch. That is a typical
depth-edge problem, and [Common failure images](failure-images.md#halos-at-depth-edges)
shows how to repair it with one brush stroke.

## What stacking cannot do

Focus stacking only rearranges what the frames contain. Keep its limits in
mind when you shoot:

- **It cannot invent detail.** An area that is not sharp in any frame stays
  soft in the composite. The steps between frames must be small enough for
  the sharp slices to overlap; a missing step shows up as a
  [focus gap](failure-images.md#focus-gaps).
- **It assumes nothing moves.** Alignment corrects the whole frame being
  shifted, turned or scaled. It cannot correct a leg, a leaf or an antenna
  that moved between frames while the rest stood still. See
  [motion](failure-images.md#motion-between-frames).
- **Depth edges are hard.** Where a near object stands in front of a distant
  background, the near object's blur spreads over the background in some
  frames, and no frame shows that strip of background cleanly. The result can
  be a glow or a ghost outline, a [halo](failure-images.md#halos-at-depth-edges).
- **Each frame must be sharp where it is in focus.** Camera shake or
  diffraction softens every frame, and a stack of soft frames is a soft
  composite.

## Where to go next

- [Shooting a stack](shooting-practice.md): step size, aperture, light,
  support and subject movement.
- [Common failure images](failure-images.md): halos, motion and focus gaps,
  how to recognise them and how to repair them.
- [Your first stack](../guide/first-stack.md): open a folder of frames in
  FocusForge and fuse it.
