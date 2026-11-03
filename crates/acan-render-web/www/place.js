// The overlap rules for the HTML marks, with no DOM, so Node can test them. marks.js measures the
// elements and applies what these return.

// Whether two boxes `{ left, right, top, bottom }` share any area.
export function overlaps(a, b) {
  return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
}

// The box an element of size w by h takes at (left, top), grown by `pad` on every side.
export function boxAt(left, top, w, h, pad) {
  return { left: left - pad, right: left + w + pad, top: top - pad, bottom: top + h + pad };
}

// Whether a box is at least partly inside a viewport of the given size.
export function inView(box, width, height) {
  return box.right >= 0 && box.left <= width && box.bottom >= 0 && box.top <= height;
}

// The top at which a box of size w by h, starting at `top`, clears every box in `placed`. It
// moves up past each box it runs into, so it ends clear of all of them. It only ever moves up.
export function clearOf(left, top, w, h, placed, pad) {
  let y = top;
  let moved = true;
  while (moved) {
    moved = false;
    for (const box of placed) {
      if (overlaps(boxAt(left, y, w, h, pad), box)) {
        y = box.top - pad - h;
        moved = true;
      }
    }
  }
  return y;
}

// The top nearest `top` at which a box of size w by h, at `left`, lies wholly in the view and
// clears every box in `placed`. It may move up or down, but not below `maxTop`. The free tops end
// at the edges of the boxes, so the nearest one is `top` itself or just above or below a box. A
// tie goes up. Returns null when no such top exists.
export function nearestClear(left, top, w, h, placed, pad, width, height, maxTop = Infinity) {
  const candidates = [top];
  for (const box of placed) candidates.push(box.top - pad - h, box.bottom + pad);
  let found = null;
  for (const y of candidates) {
    const wholly = left >= 0 && left + w <= width && y >= 0 && y + h <= height;
    if (!wholly || y > maxTop || placed.some((other) => overlaps(boxAt(left, y, w, h, pad), other))) {
      continue;
    }
    const distance = Math.abs(y - top);
    if (found === null || distance < found.distance || (distance === found.distance && y < found.y)) {
      found = { y, distance };
    }
  }
  return found === null ? null : found.y;
}

// The spots for the names the wasm side left out. It drops a name whose spots under and above
// its machine are both covered by other machines, but every machine on screen keeps its name, so
// such a machine gets a spot under its footprint's front corner, where the wasm side hangs a name.
// `machines` are `{ index, quad }`, each footprint as four screen corners, and `named` the
// indexes the wasm side placed. A footprint narrower than `minPx` on screen, or with its spot out
// of view, gets none. The page places these last, so they never push out a name the wasm side
// placed.
export function unnamedSpots(machines, named, width, height, minPx) {
  const spots = [];
  for (const { index, quad } of machines) {
    if (named.has(index)) continue;
    const xs = quad.map((p) => p[0]);
    if (Math.max(...xs) - Math.min(...xs) < minPx) continue;
    const x = xs.reduce((sum, v) => sum + v, 0) / xs.length;
    const y = Math.max(...quad.map((p) => p[1]));
    if (x < 0 || x > width || y < 0 || y > height) continue;
    spots.push({ index, x, y });
  }
  return spots;
}

// The leader a plot flag draws once it has moved off its natural top: a line from the edge of its
// box that faces the anchor (x, y), the machine's top point, to that anchor. Null when it stays.
export function leaderOf(plot, top) {
  if (top === plot.top) return null;
  const x = Math.min(Math.max(plot.x, plot.left), plot.left + plot.w);
  const y1 = plot.y < top ? top : top + plot.h;
  return { x1: x, y1, x2: plot.x, y2: plot.y };
}

// Decides what shows, in one pass. `placed` holds the boxes already placed: the page's fixed
// panels, the drones, and the flags that stay where they are. `labels` are `{ box }` in priority
// order. A label shows when it is in view and clear of everything placed before it, and a shown
// label is placed in turn. `plots` are `{ left, top, w, h, x, y }` at their natural spots, where
// (x, y) is the anchor the flag points to. They are placed after the labels, so a plot flag moves
// clear of every label, panel and flag, and never hides one. Returns `labelShown`, one boolean per
// label, `plotTops`, one top per plot, and `plotLeaders`, a leader per plot that moved, else null.
export function placeOverlays({ placed, labels, plots, width, height, pad }) {
  const boxes = [...placed];
  const labelShown = labels.map(({ box }) => {
    const clear = inView(box, width, height) && boxes.every((other) => !overlaps(box, other));
    if (clear) boxes.push(box);
    return clear;
  });
  const plotTops = [];
  const plotLeaders = [];
  for (const plot of plots) {
    const { left, top, w, h } = plot;
    // A flag stays above its anchor when it can, so its leader runs up to the machine and is not
    // hidden under the flag. Only when no clear spot is above does it take the nearest one below.
    const y = nearestClear(left, top, w, h, boxes, pad, width, height, plot.y - h)
      ?? nearestClear(left, top, w, h, boxes, pad, width, height)
      ?? clearOf(left, top, w, h, boxes, pad);
    boxes.push(boxAt(left, y, w, h, pad));
    plotTops.push(y);
    plotLeaders.push(leaderOf(plot, y));
  }
  return { labelShown, plotTops, plotLeaders };
}
