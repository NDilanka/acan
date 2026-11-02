// Node tests for the HTML mark placement rules. Run with:
//   node --test crates/acan-render-web/tests/js/place.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { boxAt, clearOf, overlaps, placeOverlays } from "../../www/place.js";

const PAD = 4;
const VIEW = { width: 800, height: 600 };

// A label of width 60 and height 14 whose anchor is (x, y), as marks.js places it.
function labelBox(x, y) {
  return boxAt(x - 30, y + 6, 60, 14, PAD);
}

// A plot flag of size w by h at (left, top), whose anchor sits 6px below its bottom edge, as
// marks.js places it.
function plotAt(left, top, w = 120, h = 30) {
  return { left, top, w, h, x: left + w / 2, y: top + h + 6 };
}

// A fixed panel, such as the order card or the key bar, as a box in view coordinates.
function panel(left, top, right, bottom) {
  return { left, right, top, bottom };
}

test("a plot flag over a visible label moves up clear of it, and the label stays shown", () => {
  const label = labelBox(100, 200);
  const plot = plotAt(80, 190);
  assert.ok(overlaps(boxAt(plot.left, plot.top, plot.w, plot.h, PAD), label));

  const { labelShown, plotTops } = placeOverlays({
    placed: [],
    labels: [{ box: label }],
    plots: [plot],
    ...VIEW,
    pad: PAD,
  });

  assert.deepEqual(labelShown, [true], "the label wins");
  assert.ok(plotTops[0] < plot.top, "the plot flag moved up");
  assert.equal(overlaps(boxAt(plot.left, plotTops[0], plot.w, plot.h, PAD), label), false);
});

test("a plot flag clear of every label stays at its natural spot", () => {
  const plot = plotAt(500, 400);
  const { plotTops, plotLeaders } = placeOverlays({
    placed: [],
    labels: [{ box: labelBox(100, 200) }],
    plots: [plot],
    ...VIEW,
    pad: PAD,
  });
  assert.equal(plotTops[0], plot.top);
  assert.equal(plotLeaders[0], null, "a flag that stays draws no leader");
});

test("a label under a drone or a flag that stays put is still hidden", () => {
  const label = labelBox(100, 200);
  const drone = { left: 90, right: 110, top: 195, bottom: 215 };
  const { labelShown } = placeOverlays({
    placed: [drone],
    labels: [{ box: label }],
    plots: [],
    ...VIEW,
    pad: PAD,
  });
  assert.deepEqual(labelShown, [false]);
});

test("a label outside the view is hidden", () => {
  const { labelShown } = placeOverlays({
    placed: [],
    labels: [{ box: labelBox(900, 200) }],
    plots: [],
    ...VIEW,
    pad: PAD,
  });
  assert.deepEqual(labelShown, [false]);
});

test("two plot flags over the same spot stack, the second above the first", () => {
  const first = plotAt(300, 300);
  const second = plotAt(300, 300);
  const { plotTops } = placeOverlays({
    placed: [],
    labels: [],
    plots: [first, second],
    ...VIEW,
    pad: PAD,
  });
  assert.equal(plotTops[0], first.top);
  const firstBox = boxAt(first.left, plotTops[0], first.w, first.h, PAD);
  const secondBox = boxAt(second.left, plotTops[1], second.w, second.h, PAD);
  assert.equal(overlaps(firstBox, secondBox), false);
  assert.ok(plotTops[1] < plotTops[0]);
});

test("clearOf ends clear of every box and only ever moves up", () => {
  const placed = [
    boxAt(0, 100, 50, 20, PAD),
    boxAt(0, 60, 50, 20, PAD),
  ];
  const top = clearOf(0, 110, 50, 20, placed, PAD);
  assert.ok(top <= 110);
  const box = boxAt(0, top, 50, 20, PAD);
  for (const other of placed) assert.equal(overlaps(box, other), false);
});

test("a panel blocks a plot flag: the flag moves clear of the panel", () => {
  const card = panel(60, 180, 400, 215);
  const plot = plotAt(80, 190);
  const { plotTops } = placeOverlays({
    placed: [card],
    labels: [],
    plots: [plot],
    ...VIEW,
    pad: PAD,
  });
  const box = boxAt(plot.left, plotTops[0], plot.w, plot.h, PAD);
  assert.equal(overlaps(box, card), false, "the flag does not sit under the panel");
});

test("a panel blocks a label: the label stays hidden under it", () => {
  const card = panel(60, 190, 400, 230);
  const { labelShown } = placeOverlays({
    placed: [card],
    labels: [{ box: labelBox(100, 200) }],
    plots: [],
    ...VIEW,
    pad: PAD,
  });
  assert.deepEqual(labelShown, [false]);
});

test("a flag under a top bar goes below it, not up off the canvas", () => {
  const bar = panel(0, 20, 800, 60);
  const plot = plotAt(300, 40);
  const { plotTops } = placeOverlays({
    placed: [bar],
    labels: [],
    plots: [plot],
    ...VIEW,
    pad: PAD,
  });
  assert.equal(plotTops[0], 60 + PAD, "the flag clears the bar from below");
  assert.ok(plotTops[0] >= 0, "the flag stays wholly in view");
});

test("a moved flag records a leader from its box to its anchor", () => {
  const label = labelBox(100, 200);
  const plot = plotAt(80, 190);
  const { plotTops, plotLeaders } = placeOverlays({
    placed: [],
    labels: [{ box: label }],
    plots: [plot],
    ...VIEW,
    pad: PAD,
  });
  const top = plotTops[0];
  assert.ok(top < plot.top, "the flag moved");
  assert.deepEqual(plotLeaders[0], {
    x1: plot.x,
    y1: top + plot.h,
    x2: plot.x,
    y2: plot.y,
  }, "the leader leaves the bottom edge of the flag and ends at its anchor");
});

test("a plot flag with a clear spot above its anchor does not drop onto the machine", () => {
  // A name just under the natural spot: the nearest free top is below it, past the anchor.
  const name = { left: 60, right: 200, top: 186, bottom: 196 };
  const plot = plotAt(80, 190);
  const { plotTops, plotLeaders } = placeOverlays({
    placed: [name],
    labels: [],
    plots: [plot],
    ...VIEW,
    pad: PAD,
  });
  assert.ok(plotTops[0] + plot.h <= plot.y, "the flag's bottom stays above its anchor");
  assert.equal(plotTops[0], 186 - PAD - plot.h, "it takes the spot above the name");
  assert.equal(plotLeaders[0].y1, plotTops[0] + plot.h, "its leader runs down from the flag");
});

test("a flag moved below its anchor starts its leader on its top edge", () => {
  const bar = panel(0, 20, 800, 60);
  const plot = { left: 300, top: 40, w: 120, h: 30, x: 360, y: 20 };
  const { plotTops, plotLeaders } = placeOverlays({
    placed: [bar],
    labels: [],
    plots: [plot],
    ...VIEW,
    pad: PAD,
  });
  assert.equal(plotTops[0], 60 + PAD);
  assert.deepEqual(plotLeaders[0], { x1: 360, y1: 64, x2: 360, y2: 20 });
});
