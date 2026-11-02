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

test("a plot flag over a visible label moves up clear of it, and the label stays shown", () => {
  const label = labelBox(100, 200);
  const plot = { left: 80, top: 190, w: 120, h: 30 };
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
  const plot = { left: 500, top: 400, w: 120, h: 30 };
  const { plotTops } = placeOverlays({
    placed: [],
    labels: [{ box: labelBox(100, 200) }],
    plots: [plot],
    ...VIEW,
    pad: PAD,
  });
  assert.equal(plotTops[0], plot.top);
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
  const first = { left: 300, top: 300, w: 120, h: 30 };
  const second = { ...first };
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
